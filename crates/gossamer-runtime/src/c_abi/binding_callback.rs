//! Gossamer closures handed to a `[rust-bindings]` function as callbacks.
//!
//! A compiled closure is an environment whose first word is the address of
//! its code, called as `extern "C" fn(env, args...) -> ret` with each argument
//! in the register class of its type. The caller registers the closure with
//! the signature classes it was compiled against, the binding receives the
//! handle, and a call through the handle converts the binding's tagged wire
//! values into those classes and back.

use std::collections::HashMap;
use std::ffi::{CStr, c_char};
use std::sync::OnceLock;

use parking_lot::Mutex;

use super::binding_wire::{GosVariantValue, wire_tag};

/// Wire tag for the unit value a callback answers when its closure returns
/// nothing.
const WIRE_TAG_UNIT: i32 = 9;

/// How a closure takes or answers one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// An integer word.
    Int,
    /// A float, in a float register.
    Float,
    /// A `bool`.
    Bool,
    /// A `char`, as its code point.
    Char,
    /// A runtime `String`.
    Str,
    /// Nothing; only a return has this class.
    Unit,
}

impl Class {
    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            b'i' => Self::Int,
            b'f' => Self::Float,
            b'b' => Self::Bool,
            b'c' => Self::Char,
            b's' => Self::Str,
            b'u' => Self::Unit,
            _ => return None,
        })
    }
}

/// One registered closure: its environment and the classes it was compiled
/// against.
struct ClosureCallback {
    env: *const u8,
    params: Vec<Class>,
    ret: Class,
}

/// Parses a signature written as the parameter class codes, `>`, and the
/// return class code (`"if>b"` for `|n: i64, x: f64| -> bool`).
fn parse_signature(text: &str) -> Option<(Vec<Class>, Class)> {
    let (params, ret) = text.split_once('>')?;
    let params = params
        .bytes()
        .map(Class::from_code)
        .collect::<Option<Vec<_>>>()?;
    if params.contains(&Class::Unit) || params.len() > MAX_ARITY {
        return None;
    }
    let [ret] = ret.as_bytes() else {
        return None;
    };
    Some((params, Class::from_code(*ret)?))
}

/// The most parameters a registered closure may take.
const MAX_ARITY: usize = 4;

/// Registered closure contexts by handle, so a release frees the one the
/// handle names.
fn contexts() -> &'static Mutex<HashMap<u64, usize>> {
    static CONTEXTS: OnceLock<Mutex<HashMap<u64, usize>>> = OnceLock::new();
    CONTEXTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Registers the closure `env`, compiled against `signature`, as a binding
/// callback. Answers the handle the binding receives, or `0` for a signature
/// the runtime cannot call through.
///
/// # Safety
/// `env` is a live closure environment for the duration of the registration,
/// and `signature` is a NUL-terminated class string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_callback_register(
    env: *const u8,
    signature: *const c_char,
) -> u64 {
    ffi_entry!(0, {
        if env.is_null() || signature.is_null() {
            return 0;
        }
        let text = unsafe { super::string::gos_str_arg_text(signature) };
        let Some((params, ret)) = parse_signature(text) else {
            return 0;
        };
        let context = Box::into_raw(Box::new(ClosureCallback { env, params, ret }));
        let handle =
            super::signal::gos_rt_callback_register(context.cast_const().cast(), invoke_closure);
        contexts().lock().insert(handle, context as usize);
        handle
    })
}

/// Ends a registration made by [`gos_rt_binding_callback_register`]. After
/// this returns the handle calls nothing, and no call through it is running.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_binding_callback_release(handle: u64) {
    ffi_entry!((), {
        if handle == 0 {
            return;
        }
        super::signal::gos_rt_callback_unregister(handle);
        if let Some(context) = contexts().lock().remove(&handle) {
            // SAFETY: the context was boxed at registration, and unregistering
            // waited for every call through the handle to leave it.
            drop(unsafe { Box::from_raw(context as *mut ClosureCallback) });
        }
    });
}

/// An argument in the register class its closure parameter takes.
#[derive(Clone, Copy)]
enum Arg {
    Word(i64),
    Float(f64),
}

/// Calls the closure code at `code` with `env` and `args`, answering `R`.
///
/// # Safety
/// `code` was compiled as `extern "C" fn(env, ..) -> R` with parameters in
/// exactly the register classes `args` carries.
unsafe fn call_closure<R>(code: *const (), env: *const u8, args: &[Arg]) -> Option<R> {
    use Arg::{Float as F, Word as W};
    macro_rules! call {
        ($($ty:ty => $value:expr),*) => {{
            // SAFETY: the caller guarantees the code's parameter classes.
            let f: unsafe extern "C" fn(*const u8 $(, $ty)*) -> R =
                unsafe { std::mem::transmute(code) };
            Some(unsafe { f(env $(, $value)*) })
        }};
    }
    match *args {
        [] => call!(),
        [W(a)] => call!(i64 => a),
        [F(a)] => call!(f64 => a),
        [W(a), W(b)] => call!(i64 => a, i64 => b),
        [W(a), F(b)] => call!(i64 => a, f64 => b),
        [F(a), W(b)] => call!(f64 => a, i64 => b),
        [F(a), F(b)] => call!(f64 => a, f64 => b),
        [W(a), W(b), W(c)] => call!(i64 => a, i64 => b, i64 => c),
        [W(a), W(b), F(c)] => call!(i64 => a, i64 => b, f64 => c),
        [W(a), F(b), W(c)] => call!(i64 => a, f64 => b, i64 => c),
        [W(a), F(b), F(c)] => call!(i64 => a, f64 => b, f64 => c),
        [F(a), W(b), W(c)] => call!(f64 => a, i64 => b, i64 => c),
        [F(a), W(b), F(c)] => call!(f64 => a, i64 => b, f64 => c),
        [F(a), F(b), W(c)] => call!(f64 => a, f64 => b, i64 => c),
        [F(a), F(b), F(c)] => call!(f64 => a, f64 => b, f64 => c),
        [W(a), W(b), W(c), W(d)] => call!(i64 => a, i64 => b, i64 => c, i64 => d),
        [W(a), W(b), W(c), F(d)] => call!(i64 => a, i64 => b, i64 => c, f64 => d),
        [W(a), W(b), F(c), W(d)] => call!(i64 => a, i64 => b, f64 => c, i64 => d),
        [W(a), W(b), F(c), F(d)] => call!(i64 => a, i64 => b, f64 => c, f64 => d),
        [W(a), F(b), W(c), W(d)] => call!(i64 => a, f64 => b, i64 => c, i64 => d),
        [W(a), F(b), W(c), F(d)] => call!(i64 => a, f64 => b, i64 => c, f64 => d),
        [W(a), F(b), F(c), W(d)] => call!(i64 => a, f64 => b, f64 => c, i64 => d),
        [W(a), F(b), F(c), F(d)] => call!(i64 => a, f64 => b, f64 => c, f64 => d),
        [F(a), W(b), W(c), W(d)] => call!(f64 => a, i64 => b, i64 => c, i64 => d),
        [F(a), W(b), W(c), F(d)] => call!(f64 => a, i64 => b, i64 => c, f64 => d),
        [F(a), W(b), F(c), W(d)] => call!(f64 => a, i64 => b, f64 => c, i64 => d),
        [F(a), W(b), F(c), F(d)] => call!(f64 => a, i64 => b, f64 => c, f64 => d),
        [F(a), F(b), W(c), W(d)] => call!(f64 => a, f64 => b, i64 => c, i64 => d),
        [F(a), F(b), W(c), F(d)] => call!(f64 => a, f64 => b, i64 => c, f64 => d),
        [F(a), F(b), F(c), W(d)] => call!(f64 => a, f64 => b, f64 => c, i64 => d),
        [F(a), F(b), F(c), F(d)] => call!(f64 => a, f64 => b, f64 => c, f64 => d),
        _ => None,
    }
}

/// The callback table's entry point for a registered closure: converts the
/// wire arguments, calls the closure, and writes its result.
extern "C" fn invoke_closure(
    context: *const u8,
    args: *const u8,
    args_len: u32,
    result_out: *mut u8,
) -> i32 {
    // SAFETY: the table hands back the context registered with this function.
    let callback = unsafe { &*context.cast::<ClosureCallback>() };
    if args_len as usize != callback.params.len() {
        return 2;
    }
    let wire: &[GosVariantValue] = if args_len == 0 {
        &[]
    } else {
        // SAFETY: the binding passes `args_len` wire values at `args`.
        unsafe { std::slice::from_raw_parts(args.cast::<GosVariantValue>(), args_len as usize) }
    };
    let mut owned_strings: Vec<*mut c_char> = Vec::new();
    let mut call_args: Vec<Arg> = Vec::with_capacity(wire.len());
    for (value, class) in wire.iter().zip(&callback.params) {
        let arg = match (class, value.tag) {
            (Class::Int, wire_tag::I64) | (Class::Bool, wire_tag::BOOL) => {
                Arg::Word(value.data as i64)
            }
            (Class::Char, wire_tag::CHAR) => Arg::Word(i64::from(value.data as u32)),
            (Class::Float, wire_tag::F64) => Arg::Float(f64::from_bits(value.data)),
            (Class::Str, wire_tag::STRING) => {
                let ptr = value.data as usize as *const c_char;
                let bytes = if ptr.is_null() {
                    &[][..]
                } else {
                    // HOST-CSTRING: the native Rust binding owns this pointer,
                    // a NUL-terminated C string it keeps alive for the call.
                    // SAFETY: as above.
                    unsafe { CStr::from_ptr(ptr) }.to_bytes()
                };
                let text = super::string::alloc_cstring(bytes);
                owned_strings.push(text);
                Arg::Word(text as i64)
            }
            _ => {
                release_strings(&owned_strings);
                return 3;
            }
        };
        call_args.push(arg);
    }
    // SAFETY: the environment's first word is its closure's code address.
    let code = unsafe { callback.env.cast::<*const ()>().read() };
    let env = callback.env;
    // SAFETY: the code was compiled against `callback.params` and
    // `callback.ret`, which the registration recorded.
    let (tag, data) = unsafe {
        match callback.ret {
            Class::Int => {
                call_closure::<i64>(code, env, &call_args).map(|v| (wire_tag::I64, v as u64))
            }
            Class::Float => {
                call_closure::<f64>(code, env, &call_args).map(|v| (wire_tag::F64, v.to_bits()))
            }
            Class::Bool => {
                call_closure::<bool>(code, env, &call_args).map(|v| (wire_tag::BOOL, u64::from(v)))
            }
            Class::Char => {
                call_closure::<u32>(code, env, &call_args).map(|v| (wire_tag::CHAR, u64::from(v)))
            }
            Class::Str => call_closure::<*mut c_char>(code, env, &call_args)
                .map(|v| (wire_tag::STRING, v as usize as u64)),
            Class::Unit => call_closure::<()>(code, env, &call_args).map(|()| (WIRE_TAG_UNIT, 0)),
        }
    }
    .unwrap_or((WIRE_TAG_UNIT, 0));
    release_strings(&owned_strings);
    if !result_out.is_null() {
        // SAFETY: the binding hands a writable wire value for the result.
        unsafe {
            result_out
                .cast::<GosVariantValue>()
                .write(GosVariantValue { tag, pad: 0, data });
        }
    }
    0
}

/// Releases the runtime strings built for a call's arguments, which the
/// closure borrowed.
fn release_strings(strings: &[*mut c_char]) {
    for text in strings {
        // SAFETY: each was allocated by `alloc_cstring` for this call.
        unsafe { super::string::gos_rt_str_free(*text) };
    }
}

#[cfg(test)]
mod tests {
    use super::{Class, parse_signature};

    #[test]
    fn signature_names_each_parameter_and_the_return() {
        assert_eq!(
            parse_signature("if>b"),
            Some((vec![Class::Int, Class::Float], Class::Bool))
        );
        assert_eq!(parse_signature(">u"), Some((Vec::new(), Class::Unit)));
        assert_eq!(parse_signature("u>i"), None);
        assert_eq!(parse_signature("iiiii>i"), None);
        assert_eq!(parse_signature("i"), None);
    }
}
