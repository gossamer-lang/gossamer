//! Binding items as VM natives: registered from cached signatures, their
//! library loaded on the first call, called through their VM thunks.

#![allow(
    unsafe_code,
    reason = "calls binding thunks and answers their callbacks"
)]

use std::ffi::c_void;
use std::sync::Arc;

use gossamer_binding_abi::{
    AbiValue, CallCtx, CallOut, HostTable, STATUS_ERROR, STATUS_OK, STATUS_PANIC, VmThunk,
};
use gossamer_interp::value::{NativeDispatch, RuntimeError, RuntimeResult, Value};
use gossamer_resolve::BindingType;
use parking_lot::RwLock;

use crate::library::LazyLibrary;
use crate::marshal::{Encoded, decode, message_of};

/// The host table `gos` hands every library: the runtime's, with the VM's
/// callback entries.
pub(crate) static HOST_TABLE: HostTable = HostTable {
    vm_invoke: Some(vm_invoke),
    vm_free_out: Some(vm_free_out),
    vm_callback_retain: Some(vm_callback_retain),
    vm_callback_release: Some(vm_callback_release),
    ..gossamer_runtime::c_abi::binding_host::compiled_host_table()
};

/// One registered item.
struct Item {
    library: Arc<LazyLibrary>,
    module: String,
    name: String,
    params: Vec<BindingType>,
    thunk: std::sync::OnceLock<Result<(VmThunk, unsafe extern "C" fn(*mut c_void)), String>>,
}

impl Item {
    fn thunk(&self) -> Result<(VmThunk, unsafe extern "C" fn(*mut c_void)), String> {
        self.thunk
            .get_or_init(|| {
                let loaded = self.library.get()?;
                let item = loaded
                    .items
                    .get(&(self.module.clone(), self.name.clone()))
                    .ok_or_else(|| {
                        format!(
                            "the binding library at {} no longer has {}::{}",
                            self.library.path.display(),
                            self.module,
                            self.name
                        )
                    })?;
                Ok((item.vm, loaded.free_out))
            })
            .clone()
    }
}

static ITEMS: RwLock<Vec<Arc<Item>>> = RwLock::new(Vec::new());
static LEAF_GROUPS: RwLock<Vec<Vec<usize>>> = RwLock::new(Vec::new());

/// The VM's state for one call, which a callback the binding makes reaches.
struct Ctx<'a> {
    dispatch: &'a mut dyn NativeDispatch,
}

/// Registers `library`'s items as VM natives under `module::item`, and
/// answers each item's index for its leaf name.
pub(crate) fn register(
    library: Arc<LazyLibrary>,
    signatures: &crate::Signatures,
) -> Vec<(String, usize)> {
    let mut leaves = Vec::new();
    let mut items = ITEMS.write();
    for module in &signatures.modules {
        for item in &module.items {
            let index = items.len();
            items.push(Arc::new(Item {
                library: Arc::clone(&library),
                module: module.path.clone(),
                name: item.name.clone(),
                params: item
                    .params
                    .iter()
                    .map(crate::signatures::SigType::to_binding_type)
                    .collect(),
                thunk: std::sync::OnceLock::new(),
            }));
            let qualified: &'static str =
                Box::leak(format!("{}::{}", module.path, item.name).into_boxed_str());
            gossamer_interp::register_external_value(
                qualified,
                Value::native_bound(qualified, call_item, index),
            );
            leaves.push((item.name.clone(), index));
        }
    }
    leaves
}

/// Registers each leaf name: one item's leaf calls it, and a leaf several
/// modules share calls the one whose arity the call matches.
pub(crate) fn register_leaves(leaves: Vec<(String, usize)>) {
    let mut by_name: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (name, index) in leaves {
        by_name.entry(name).or_default().push(index);
    }
    let mut groups = LEAF_GROUPS.write();
    for (name, indices) in by_name {
        let leaf: &'static str = Box::leak(name.into_boxed_str());
        let value = if let [only] = indices.as_slice() {
            Value::native_bound(leaf, call_item, *only)
        } else {
            groups.push(indices);
            Value::native_bound(leaf, call_leaf, groups.len() - 1)
        };
        gossamer_interp::register_external_value(leaf, value);
    }
}

fn call_leaf(
    group: usize,
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let candidates = LEAF_GROUPS.read().get(group).cloned().unwrap_or_default();
    let items = ITEMS.read();
    let chosen = candidates
        .iter()
        .copied()
        .find(|index| {
            items
                .get(*index)
                .is_some_and(|item| item.params.len() == args.len())
        })
        .or_else(|| candidates.first().copied());
    drop(items);
    match chosen {
        Some(index) => call_item(index, dispatch, args),
        None => Err(RuntimeError::Type(
            "no binding item answers this name".to_string(),
        )),
    }
}

fn call_item(
    index: usize,
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let Some(item) = ITEMS.read().get(index).cloned() else {
        return Err(RuntimeError::Type(format!("no binding item {index}")));
    };
    if args.len() != item.params.len() {
        return Err(RuntimeError::Arity {
            expected: item.params.len(),
            found: args.len(),
        });
    }
    let (thunk, free_out) = item.thunk().map_err(RuntimeError::Type)?;
    let mut storage = Encoded::default();
    let encoded = args
        .iter()
        .zip(&item.params)
        .map(|(arg, ty)| storage.encode(arg, Some(ty)))
        .collect::<RuntimeResult<Vec<AbiValue>>>()?;
    let mut ctx = Ctx { dispatch };
    let mut out = CallOut::EMPTY;
    // SAFETY: `encoded`, the storage it points into, and `args` outlive the
    // call; `out` is a local; `ctx` is this call's context until it returns.
    let status = unsafe {
        thunk(
            (&raw mut ctx).cast::<CallCtx>(),
            encoded.as_ptr(),
            encoded.len(),
            &raw mut out,
        )
    };
    // SAFETY: the thunk wrote `out`, whose storage it owns until freed below.
    let answer = unsafe { decode(&out.value) };
    // SAFETY: the owner the thunk wrote, handed back once.
    unsafe { free_out(out.owner) };
    // The call's shares of its callbacks end here, after the answer, which
    // may name one, is read.
    drop(storage);
    let answer = answer?;
    match status {
        STATUS_OK => Ok(answer),
        STATUS_PANIC => Err(RuntimeError::Panic(format!(
            "{}::{}: {}",
            item.module,
            item.name,
            message_of(&answer)
        ))),
        STATUS_ERROR => Err(RuntimeError::Type(format!(
            "{}::{}: {}",
            item.module,
            item.name,
            message_of(&answer)
        ))),
        other => Err(RuntimeError::Type(format!(
            "{}::{} answered status {other}",
            item.module, item.name
        ))),
    }
}

/// What the host answers into a callback's [`CallOut`]: the encoded value and
/// the storage it points into.
struct Answer {
    _storage: Encoded,
    _value: Box<Value>,
}

/// Writes `value` into `out` as host-owned storage.
///
/// # Safety
/// `out` is writable.
unsafe fn write_answer(out: *mut CallOut, value: Value) -> RuntimeResult<()> {
    let value = Box::new(value);
    let mut storage = Encoded::default();
    let encoded = storage.encode(&value, None)?;
    let owner = Box::into_raw(Box::new(Answer {
        _storage: storage,
        _value: value,
    }));
    // SAFETY: the caller's contract.
    unsafe {
        out.write(CallOut {
            value: encoded,
            owner: owner.cast(),
        });
    }
    Ok(())
}

unsafe extern "C" fn vm_invoke(
    ctx: *mut CallCtx,
    handle: u64,
    args: *const AbiValue,
    arg_count: usize,
    out: *mut CallOut,
) -> u32 {
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> RuntimeResult<Value> {
            let callable = crate::marshal::callable(handle).ok_or_else(|| {
                RuntimeError::Type(format!("callback {handle} is no longer held"))
            })?;
            let raw = if args.is_null() || arg_count == 0 {
                &[][..]
            } else {
                // SAFETY: the binding passes `arg_count` live values.
                unsafe { std::slice::from_raw_parts(args, arg_count) }
            };
            // SAFETY: as above.
            let decoded = raw
                .iter()
                .map(|v| unsafe { decode(v) })
                .collect::<RuntimeResult<Vec<_>>>()?;
            if ctx.is_null() {
                // A null context comes from a binding compiled code called,
                // which the VM active on this thread entered.
                return gossamer_interp::call_value_on_running_vm(&callable, decoded)
                    .unwrap_or_else(|| {
                        Err(RuntimeError::Type(
                            "a VM callable was called with no VM call running".to_string(),
                        ))
                    });
            }
            // SAFETY: a non-null `ctx` is the `Ctx` of the VM call running on
            // this thread, which the binding passes back while that call runs.
            let ctx = unsafe { &mut *ctx.cast::<Ctx<'_>>() };
            ctx.dispatch.call_value(&callable, decoded)
        }));
    let (status, value) = match outcome {
        Ok(Ok(value)) => (STATUS_OK, value),
        Ok(Err(RuntimeError::Panic(message))) => (STATUS_PANIC, Value::String(message.into())),
        Ok(Err(err)) => (STATUS_ERROR, Value::String(err.to_string().into())),
        Err(_) => (
            STATUS_PANIC,
            Value::String("a callback panicked inside the host".into()),
        ),
    };
    // SAFETY: the binding passes a writable answer.
    match unsafe { write_answer(out, value) } {
        Ok(()) => status,
        Err(err) => {
            // SAFETY: as above; a string always encodes.
            let _ = unsafe { write_answer(out, Value::String(err.to_string().into())) };
            STATUS_ERROR
        }
    }
}

unsafe extern "C" fn vm_free_out(owner: *mut c_void) {
    if !owner.is_null() {
        // SAFETY: `owner` is what `write_answer` boxed, handed back once.
        drop(unsafe { Box::from_raw(owner.cast::<Answer>()) });
    }
}

unsafe extern "C" fn vm_callback_retain(handle: u64) {
    crate::marshal::retain_callable(handle);
}

unsafe extern "C" fn vm_callback_release(handle: u64) {
    crate::marshal::release_callable(handle);
}
