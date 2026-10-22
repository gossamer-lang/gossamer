//! The VM entry of binding items: [`AbiValue`] arguments decoded into
//! [`Value`]s, the answer encoded into storage this library owns until the
//! host hands it back through [`free_out`], and the [`VmDispatch`] a
//! `cb_fn` calls Gossamer code back through.

#![allow(unsafe_code, reason = "reads and writes the C values of a VM call")]

use std::ffi::c_void;
use std::sync::Arc;

use gossamer_binding_abi::{
    AbiValue, CallCtx, CallOut, STATUS_ERROR, STATUS_OK, STATUS_PANIC, tag,
};

use crate::value::{
    CallbackOrigin, CallbackRef, NativeDispatch, RuntimeError, RuntimeResult, Str, Value,
};

/// The storage an encoded value points into.
#[derive(Default)]
struct Encoded {
    arrays: Vec<Box<[AbiValue]>>,
    texts: Vec<Box<[u8]>>,
}

impl Encoded {
    fn text(&mut self, tag: u32, bytes: Box<[u8]>) -> AbiValue {
        let value = AbiValue::pointing(tag, bytes.as_ptr(), bytes.len());
        self.texts.push(bytes);
        value
    }

    fn array(&mut self, tag: u32, items: Vec<AbiValue>, count: usize) -> AbiValue {
        let items = items.into_boxed_slice();
        let value = AbiValue::pointing(tag, items.as_ptr().cast(), count);
        self.arrays.push(items);
        value
    }

    fn encode(&mut self, value: &Value) -> AbiValue {
        match value {
            Value::Unit => AbiValue::UNIT,
            Value::Bool(b) => AbiValue::words(tag::BOOL, u64::from(*b), 0),
            Value::Int(i) => AbiValue::words(tag::INT, *i as u64, 0),
            Value::Float(f) => AbiValue::words(tag::FLOAT, f.to_bits(), 0),
            Value::Char(c) => AbiValue::words(tag::CHAR, u64::from(*c), 0),
            Value::String(s) => self.text(tag::STRING, s.as_bytes().into()),
            Value::Bytes(bytes) => self.text(tag::BYTES, bytes.as_slice().into()),
            Value::Array(items) => {
                let encoded = items.iter().map(|v| self.encode(v)).collect();
                self.array(tag::LIST, encoded, items.len())
            }
            Value::Tuple(items) => {
                let encoded = items.iter().map(|v| self.encode(v)).collect();
                self.array(tag::TUPLE, encoded, items.len())
            }
            Value::Map(entries) => {
                let mut encoded = Vec::with_capacity(entries.len() * 2);
                for (k, v) in entries.iter() {
                    encoded.push(self.encode(k));
                    encoded.push(self.encode(v));
                }
                self.array(tag::MAP, encoded, entries.len())
            }
            Value::Variant(inner) => {
                let mut encoded = Vec::with_capacity(inner.fields.len() + 1);
                encoded.push(self.text(tag::STRING, inner.name.as_bytes().into()));
                encoded.extend(inner.fields.iter().map(|v| self.encode(v)));
                let count = encoded.len();
                self.array(tag::VARIANT, encoded, count)
            }
            Value::Struct(inner) => {
                let mut encoded = Vec::with_capacity(inner.fields.len() * 2 + 1);
                encoded.push(self.text(tag::STRING, inner.name.as_bytes().into()));
                for (name, v) in &inner.fields {
                    encoded.push(self.text(tag::STRING, name.as_bytes().into()));
                    encoded.push(self.encode(v));
                }
                let count = encoded.len();
                self.array(tag::STRUCT, encoded, count)
            }
            Value::Callback(callback) => AbiValue::words(tag::CALLBACK, callback.handle, 0),
        }
    }
}

/// Reads a value the host passed or answered.
///
/// # Safety
/// `value` and everything it points at stay alive for the read.
unsafe fn decode(value: &AbiValue) -> RuntimeResult<Value> {
    let text = |value: &AbiValue| -> RuntimeResult<Str> {
        // SAFETY: the caller's contract covers every nested value.
        let bytes = unsafe { value.bytes() };
        std::str::from_utf8(bytes)
            .map(Str::from)
            .map_err(|_| RuntimeError::Type("a string that is not UTF-8".to_string()))
    };
    // SAFETY: the caller's contract covers every nested value.
    let elements = || unsafe { value.elements() };
    let decode_all = |items: &[AbiValue]| -> RuntimeResult<Vec<Value>> {
        // SAFETY: as above.
        items.iter().map(|v| unsafe { decode(v) }).collect()
    };
    Ok(match value.tag {
        tag::UNIT => Value::Unit,
        tag::BOOL => Value::Bool(value.a != 0),
        tag::INT => Value::Int(value.a as i64),
        tag::FLOAT => Value::Float(f64::from_bits(value.a)),
        tag::CHAR => Value::Char(
            u32::try_from(value.a)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| RuntimeError::Type(format!("char {} is not a scalar", value.a)))?,
        ),
        tag::STRING => Value::String(text(value)?),
        // SAFETY: as above.
        tag::BYTES => Value::Bytes(unsafe { value.bytes() }.to_vec()),
        tag::LIST => Value::Array(Arc::new(decode_all(elements())?)),
        tag::TUPLE => Value::Tuple(Arc::new(decode_all(elements())?)),
        tag::MAP => {
            let items = decode_all(elements())?;
            let mut entries = Vec::with_capacity(items.len() / 2);
            let mut iter = items.into_iter();
            while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
                entries.push((k, v));
            }
            Value::Map(Arc::new(entries))
        }
        tag::VARIANT => {
            let items = elements();
            let (name, payload) = items
                .split_first()
                .ok_or_else(|| RuntimeError::Type("a variant without a name".to_string()))?;
            Value::variant(text(name)?.as_str(), decode_all(payload)?)
        }
        tag::STRUCT => {
            let items = elements();
            let (name, rest) = items
                .split_first()
                .ok_or_else(|| RuntimeError::Type("a struct without a name".to_string()))?;
            let mut fields = Vec::with_capacity(rest.len() / 2);
            for pair in rest.chunks_exact(2) {
                // SAFETY: as above.
                fields.push((text(&pair[0])?.as_str().to_string(), unsafe {
                    decode(&pair[1])
                }?));
            }
            Value::struct_(text(name)?.as_str(), fields)
        }
        tag::CALLBACK => Value::Callback(CallbackRef {
            handle: value.a,
            origin: CallbackOrigin::Vm,
        }),
        other => return Err(RuntimeError::Type(format!("an unknown value tag {other}"))),
    })
}

/// Writes `value` into `out`, owned by this library until [`free_out`].
///
/// # Safety
/// `out` is writable.
unsafe fn write_out(out: *mut CallOut, value: &Value) {
    let mut storage = Box::<Encoded>::default();
    let encoded = storage.encode(value);
    // SAFETY: the caller's contract.
    unsafe {
        out.write(CallOut {
            value: encoded,
            owner: Box::into_raw(storage).cast(),
        });
    }
}

/// Frees the answer of a VM call by the owner it wrote.
///
/// # Safety
/// `owner` is null or the owner a VM call of this library wrote, not yet
/// freed.
pub(crate) unsafe extern "C" fn free_out(owner: *mut c_void) {
    if !owner.is_null() {
        // SAFETY: the caller's contract; `write_out` boxed it.
        drop(unsafe { Box::from_raw(owner.cast::<Encoded>()) });
    }
}

/// The message a caught panic carries.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a binding panicked".to_string()
    }
}

/// Runs a VM call of an item: decodes `arg_count` arguments at `args`, calls
/// `body` with them and the call's dispatch, and writes its answer, its
/// error, or its panic into `out`.
///
/// # Safety
/// `args` points at `arg_count` values that stay alive for the call, `out` is
/// writable, and `ctx` is the host's context for this call.
#[doc(hidden)]
pub unsafe fn vm_call(
    ctx: *mut CallCtx,
    args: *const AbiValue,
    arg_count: usize,
    out: *mut CallOut,
    body: impl FnOnce(&mut dyn NativeDispatch, &[Value]) -> RuntimeResult<Value>,
) -> u32 {
    let raw: &[AbiValue] = if args.is_null() || arg_count == 0 {
        &[]
    } else {
        // SAFETY: the caller's contract.
        unsafe { std::slice::from_raw_parts(args, arg_count) }
    };
    let _call = crate::host::InCall::enter();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: the caller's contract covers every argument.
        let decoded: RuntimeResult<Vec<Value>> = raw.iter().map(|v| unsafe { decode(v) }).collect();
        let mut dispatch = VmDispatch { ctx };
        body(&mut dispatch, &decoded?)
    }));
    let (status, value) = match outcome {
        Ok(Ok(value)) => (STATUS_OK, value),
        Ok(Err(RuntimeError::Panic(message))) => (STATUS_PANIC, Value::String(message.into())),
        Ok(Err(err)) => (STATUS_ERROR, Value::String(err.to_string().into())),
        Err(payload) => (STATUS_PANIC, Value::String(panic_message(&*payload).into())),
    };
    // SAFETY: the caller's contract.
    unsafe { write_out(out, &value) };
    status
}

/// A VM call's argument as `T`: the text a JSON-wired type crosses as, or
/// the value itself.
///
/// # Errors
/// The argument is not a `T`.
#[doc(hidden)]
pub fn vm_arg<T: crate::native::BindingAbi + crate::FromGos>(value: &Value) -> RuntimeResult<T> {
    if !T::JSON {
        return T::from_gos(value);
    }
    let Value::String(text) = value else {
        return Err(RuntimeError::Type(format!(
            "expected argument text, found {value:?}"
        )));
    };
    T::read_wire_text(text.as_str()).map_err(RuntimeError::Type)
}

/// A VM call's answer: the text a JSON-wired type crosses as, or the value
/// itself.
#[doc(hidden)]
pub fn vm_ret<T: crate::native::BindingAbi + crate::ToGos>(value: T) -> Value {
    if T::JSON {
        Value::String(value.write_wire_text().into())
    } else {
        value.to_gos()
    }
}

/// A parameter's value as the VM entry takes it.
///
/// # Errors
/// The value is not a `T`.
#[doc(hidden)]
pub fn value_to_entry<T: crate::native::BindingAbi + crate::FromGos>(
    value: &Value,
) -> RuntimeResult<Value> {
    if !T::JSON {
        return Ok(value.clone());
    }
    Ok(Value::String(T::from_gos(value)?.write_wire_text().into()))
}

/// The value a VM entry's answer stands for.
///
/// # Errors
/// The answer is not a `T`.
#[doc(hidden)]
pub fn entry_to_value<T: crate::native::BindingAbi + crate::ToGos>(
    value: &Value,
) -> RuntimeResult<Value> {
    if !T::JSON {
        return Ok(value.clone());
    }
    let Value::String(text) = value else {
        return Err(RuntimeError::Type(format!(
            "expected answer text, found {value:?}"
        )));
    };
    Ok(T::read_wire_text(text.as_str())
        .map_err(RuntimeError::Type)?
        .to_gos())
}

/// The [`crate::Signature::json`] mask of an item whose return and
/// parameters cross as text where `ret` and `params` say.
#[doc(hidden)]
#[must_use]
pub const fn json_mask(params: &[bool], ret: bool) -> u64 {
    let mut mask = ret as u64;
    let mut i = 0;
    while i < params.len() {
        if params[i] {
            mask |= 1 << (i + 1);
        }
        i += 1;
    }
    mask
}

/// Calls a VM entry with `args` and no VM to call back into, answering its
/// value, or its error or panic as a [`RuntimeError`].
pub(crate) fn call_vm_item(
    vm: gossamer_binding_abi::VmThunk,
    args: &[Value],
) -> RuntimeResult<Value> {
    let mut storage = Encoded::default();
    let encoded: Vec<AbiValue> = args.iter().map(|v| storage.encode(v)).collect();
    let mut out = CallOut::EMPTY;
    // SAFETY: `encoded` and its storage outlive the call, `out` is a local,
    // and a null context answers every VM callback with an error.
    let status = unsafe {
        vm(
            std::ptr::null_mut(),
            encoded.as_ptr(),
            encoded.len(),
            &raw mut out,
        )
    };
    // SAFETY: the entry wrote `out` and owns its storage until freed below.
    let answer = unsafe { decode(&out.value) };
    // SAFETY: the owner the entry wrote.
    unsafe { free_out(out.owner) };
    let answer = answer?;
    match status {
        STATUS_OK => Ok(answer),
        STATUS_PANIC => Err(RuntimeError::Panic(text_of(&answer))),
        _ => Err(RuntimeError::Type(text_of(&answer))),
    }
}

/// What a `cb_fn` running under the VM calls Gossamer code back through: the
/// host's context for the running call.
#[derive(Debug)]
pub struct VmDispatch {
    ctx: *mut CallCtx,
}

impl NativeDispatch for VmDispatch {
    fn invoke_callback(&mut self, callback: CallbackRef, args: Vec<Value>) -> RuntimeResult<Value> {
        if callback.origin == CallbackOrigin::Compiled {
            return crate::native::invoke_compiled_callback(callback.handle, &args);
        }
        let mut storage = Encoded::default();
        let encoded: Vec<AbiValue> = args.iter().map(|v| storage.encode(v)).collect();
        let mut out = CallOut::EMPTY;
        // SAFETY: `ctx` is this call's context, and `encoded` and its storage
        // outlive the call.
        let status = unsafe {
            crate::host::vm_invoke(
                self.ctx,
                callback.handle,
                encoded.as_ptr(),
                encoded.len(),
                &mut out,
            )
        }
        .ok_or(RuntimeError::Unsupported("a VM callback outside the VM"))?;
        // SAFETY: the host wrote `out` and keeps its storage until it is
        // handed back below.
        let answer = unsafe { decode(&out.value) };
        crate::host::vm_free_out(&out);
        let answer = answer?;
        match status {
            STATUS_OK => Ok(answer),
            STATUS_PANIC => Err(RuntimeError::Panic(text_of(&answer))),
            _ => Err(RuntimeError::Type(text_of(&answer))),
        }
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.as_str().to_string(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(value: &Value) -> Value {
        let mut out = CallOut::EMPTY;
        // SAFETY: `out` is a local; the encoded storage lives until freed.
        unsafe {
            write_out(&raw mut out, value);
            let back = decode(&out.value).unwrap();
            free_out(out.owner);
            back
        }
    }

    #[test]
    fn every_value_kind_survives_an_encode_and_a_decode() {
        let value = Value::Tuple(Arc::new(vec![
            Value::Unit,
            Value::Bool(true),
            Value::Int(-7),
            Value::Float(2.5),
            Value::Char('z'),
            Value::String("text".into()),
            Value::Bytes(vec![0, 255]),
            Value::Array(Arc::new(vec![Value::Int(1), Value::Int(2)])),
            Value::Map(Arc::new(vec![(Value::String("k".into()), Value::Int(3))])),
            Value::variant("Some", vec![Value::Int(4)]),
            Value::struct_("P", vec![("x".into(), Value::Float(1.0))]),
        ]));
        assert_eq!(round_trip(&value), value);
    }

    #[test]
    fn a_panicking_item_answers_its_message() {
        let mut out = CallOut::EMPTY;
        // SAFETY: no arguments; `out` is a local.
        let status = unsafe {
            vm_call(
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                &raw mut out,
                |_, _| panic!("no such window"),
            )
        };
        assert_eq!(status, STATUS_PANIC);
        // SAFETY: `write_out` filled `out`.
        let message = unsafe { decode(&out.value) }.unwrap();
        // SAFETY: the owner `vm_call` wrote.
        unsafe { free_out(out.owner) };
        assert_eq!(message, Value::String("no such window".into()));
    }
}
