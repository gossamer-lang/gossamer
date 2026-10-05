//! `__gos_ffi_call(symbol, signature, library, args..)`: how bytecode calls a
//! function declared in an `unsafe extern "C"` block.
//!
//! The arguments become 64-bit words, a byte buffer as a pointer to its
//! packed bytes, and a Cranelift trampoline generated for the signature makes
//! the C call. The call is bracketed by the runtime's enter and leave, which
//! mark the worker as possibly blocked and capture `errno`, exactly as the
//! compiled tiers bracket it.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use gossamer_mir::ForeignParam;
use gossamer_types::{c_class_signed, c_class_width};

use crate::value::{NativeDispatch, RuntimeError, RuntimeResult, ThreadConfinedCell, Value};

/// Resolved addresses by `(symbol, library)`, stored as integers so the
/// table is `Send`.
static RESOLVED: Mutex<Option<HashMap<(String, String), usize>>> = Mutex::new(None);

/// The native builtin behind every foreign call on the bytecode tier.
pub(crate) fn native_ffi_call(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let [symbol, signature, library, layouts, call_args @ ..] = args else {
        return Err(RuntimeError::Arity {
            expected: 4,
            found: args.len(),
        });
    };
    let (
        Value::String(symbol),
        Value::String(signature),
        Value::String(library),
        Value::String(layouts),
    ) = (symbol, signature, library, layouts)
    else {
        return Err(RuntimeError::Type(
            "__gos_ffi_call: symbol, signature, library, and layouts are strings".to_string(),
        ));
    };
    let malformed =
        || RuntimeError::Type(format!("__gos_ffi_call: malformed signature `{signature}`"));
    let (params, ret_spelling) = signature.as_str().split_once('>').ok_or_else(malformed)?;
    let params = gossamer_mir::foreign_params(params).ok_or_else(malformed)?;
    let ret_struct = match ret_spelling.strip_prefix("s(") {
        Some(inner) => Some(
            inner
                .strip_suffix(')')
                .and_then(gossamer_abi::c_aggregate::CLayout::parse)
                .ok_or_else(malformed)?,
        ),
        None => None,
    };
    let ret = ret_spelling.chars().next().ok_or_else(malformed)?;
    let mut layouts: Vec<&str> = layouts.as_str().split('|').collect();
    let (target, call_args) = if symbol.as_str() == gossamer_hir::FFI_INDIRECT_SYMBOL {
        // A call through an address carries the address first.
        let (address, forwarded) = call_args.split_first().ok_or_else(malformed)?;
        if !layouts.is_empty() {
            layouts.remove(0);
        }
        let address = match address {
            Value::Int(n) => *n as usize,
            Value::Uint(n) => *n as usize,
            other => {
                return Err(RuntimeError::Type(format!(
                    "a native function address is an integer, not `{other}`"
                )));
            }
        };
        (address, forwarded)
    } else {
        (resolve(symbol.as_str(), library.as_str())?, call_args)
    };
    // A struct result comes back through one more, final argument.
    let expected = params.len() + usize::from(ret_struct.is_some());
    if expected != call_args.len() {
        return Err(RuntimeError::Arity {
            expected,
            found: call_args.len(),
        });
    }
    let signature = Signature {
        params: &params,
        layouts: &layouts,
        ret,
        ret_struct,
    };
    callbacks::with_active(dispatch, || {
        call(symbol.as_str(), target, &signature, call_args)
    })
}

fn resolve(symbol: &str, library: &str) -> RuntimeResult<usize> {
    let key = (symbol.to_string(), library.to_string());
    let mut table = RESOLVED.lock();
    let table = table.get_or_insert_with(HashMap::new);
    if let Some(&addr) = table.get(&key) {
        return Ok(addr);
    }
    let libraries: Vec<String> = if library.is_empty() {
        Vec::new()
    } else {
        vec![library.to_string()]
    };
    let addr = platform::resolve(symbol, &libraries).ok_or_else(|| {
        RuntimeError::Panic(format!(
            "foreign function `{symbol}` was not found in the process{}",
            if library.is_empty() {
                String::new()
            } else {
                format!(" or in library `{library}`")
            }
        ))
    })?;
    table.insert(key, addr);
    Ok(addr)
}

/// A slice or struct argument: its C bytes for the call, and how they come
/// back when the callee's writes do.
struct Buffer {
    bytes: Vec<u8>,
    write_back: Option<(Arc<ThreadConfinedCell>, Shape)>,
}

/// How a buffer's bytes map onto the value they were packed from.
enum Shape {
    /// Elements of one class, in order.
    Elements(char),
    /// The leaves of a struct layout.
    Leaves(Vec<Leaf>),
}

/// One scalar of a struct layout: the fields and elements leading to it,
/// its C offset, and its class.
struct Leaf {
    steps: Vec<Step>,
    offset: usize,
    class: char,
}

#[derive(Clone, Copy)]
enum Step {
    Field(usize),
    Index(i64),
}

fn parse_layout(text: &str) -> Option<(usize, Vec<Leaf>)> {
    let (size, leaves) = text.split_once(';')?;
    let size = size.parse().ok()?;
    let mut parsed = Vec::new();
    for leaf in leaves.split(',').filter(|leaf| !leaf.is_empty()) {
        let (steps, rest) = leaf.split_once('@')?;
        let (offset, class) = rest.split_once(':')?;
        let steps = steps
            .split('.')
            .filter(|step| !step.is_empty())
            .map(|step| match step.split_at(1) {
                ("f", index) => index.parse().ok().map(Step::Field),
                ("i", index) => index.parse().ok().map(Step::Index),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        parsed.push(Leaf {
            steps,
            offset: offset.parse().ok()?,
            class: class.chars().next()?,
        });
    }
    Some((size, parsed))
}

/// A routed call's signature, as the call site spelled it.
struct Signature<'a> {
    params: &'a [ForeignParam],
    layouts: &'a [&'a str],
    ret: char,
    /// The layout of a struct result, which the final argument receives.
    ret_struct: Option<gossamer_abi::c_aggregate::CLayout>,
}

/// The C buffer for the struct argument `value` laid out by `layout` (a
/// `foreign_layouts` entry), written back to `cell` when it is given.
fn struct_buffer(
    symbol: &str,
    index: usize,
    layout: &str,
    value: &Value,
    cell: Option<Arc<ThreadConfinedCell>>,
) -> RuntimeResult<Buffer> {
    let (size, leaves) = parse_layout(layout).ok_or_else(|| {
        RuntimeError::Type(format!(
            "foreign function `{symbol}`: malformed layout for parameter {}",
            index + 1
        ))
    })?;
    let mut bytes = vec![0u8; size.max(1)];
    for leaf in &leaves {
        let scalar = leaf_value(value, &leaf.steps)?;
        let word = scalar_word(symbol, leaf.class, &scalar)?;
        write_class(&mut bytes[leaf.offset..], leaf.class, word);
    }
    Ok(Buffer {
        bytes,
        write_back: cell.map(|c| (c, Shape::Leaves(leaves))),
    })
}

fn call(
    symbol: &str,
    target: usize,
    signature: &Signature<'_>,
    args: &[Value],
) -> RuntimeResult<Value> {
    let Signature {
        params,
        layouts,
        ret,
        ..
    } = *signature;
    // A signature moving a struct by value takes a stub planned for the
    // host's calling convention.
    let plan = (signature.ret_struct.is_some()
        || params
            .iter()
            .any(|param| matches!(param, ForeignParam::ByValue(_))))
    .then(|| platform::plan(params, signature.ret_struct.as_ref(), ret))
    .transpose()?;
    let mut buffers: Vec<Buffer> = Vec::new();
    let mut kinds = Vec::with_capacity(args.len());
    let mut stub_params = Vec::with_capacity(args.len());
    let mut words = Vec::with_capacity(args.len());
    // Which word holds which buffer's address, filled once `buffers` stops
    // growing so no address moves after it is taken.
    let mut buffer_slots = Vec::new();
    for (index, (param, arg)) in params.iter().zip(args).enumerate() {
        let (value, cell) = match arg {
            Value::MutCell(cell) => (cell.lock().clone(), Some(Arc::clone(cell))),
            other => (other.clone(), None),
        };
        match param {
            ForeignParam::ByValue(_) => {
                let layout = layouts.get(index).copied().unwrap_or_default();
                buffer_slots.push((words.len(), buffers.len()));
                buffers.push(struct_buffer(symbol, index, layout, &value, None)?);
                words.push(0);
                kinds.push(platform::pointer_kind());
                stub_params.push(platform::struct_param(plan.as_ref(), index)?);
                continue;
            }
            &ForeignParam::Scalar(class) => {
                kinds.push(platform::kind(class).ok_or_else(|| unknown_class(symbol, class))?);
                words.push(scalar_word(symbol, class, &value)?);
            }
            &ForeignParam::Slice { elem, writable } => {
                kinds.push(platform::pointer_kind());
                let bytes = pack_elements(symbol, elem, &value)?;
                buffer_slots.push((words.len(), buffers.len()));
                buffers.push(Buffer {
                    bytes,
                    write_back: cell
                        .filter(|_| writable)
                        .map(|c| (c, Shape::Elements(elem))),
                });
                words.push(0);
            }
            &ForeignParam::Struct { writable } => {
                kinds.push(platform::pointer_kind());
                let layout = layouts.get(index).copied().unwrap_or_default();
                buffer_slots.push((words.len(), buffers.len()));
                buffers.push(struct_buffer(
                    symbol,
                    index,
                    layout,
                    &value,
                    cell.filter(|_| writable),
                )?);
                words.push(0);
            }
        }
        if let Some(kind) = kinds.last() {
            stub_params.push(platform::scalar_param(*kind));
        }
    }
    // A struct result is written to the buffer the final argument holds.
    if signature.ret_struct.is_some() {
        let holder = args.get(params.len()).ok_or_else(|| RuntimeError::Arity {
            expected: params.len() + 1,
            found: args.len(),
        })?;
        let (value, cell) = match holder {
            Value::MutCell(cell) => (cell.lock().clone(), Some(Arc::clone(cell))),
            other => (other.clone(), None),
        };
        let layout = layouts.get(params.len()).copied().unwrap_or_default();
        buffer_slots.push((words.len(), buffers.len()));
        buffers.push(struct_buffer(symbol, params.len(), layout, &value, cell)?);
        words.push(0);
    }
    for (word, buffer) in buffer_slots {
        words[word] = platform::buffer_address(&mut buffers[buffer].bytes);
    }
    let result = if let Some(plan) = &plan {
        platform::invoke_struct(target, &stub_params, plan, ret, &words)?
    } else {
        let ret_kind = platform::kind(ret).ok_or_else(|| unknown_class(symbol, ret))?;
        platform::invoke(target, &kinds, ret_kind, &words)?
    };
    write_back(buffers)?;
    Ok(match ret {
        'v' | 's' => Value::Unit,
        'L' => Value::Uint(result),
        class => scalar_value(class, result),
    })
}

/// Copies each writable buffer the call filled back into the `&mut`
/// argument it came from.
fn write_back(buffers: Vec<Buffer>) -> RuntimeResult<()> {
    for buffer in buffers {
        let Some((cell, shape)) = buffer.write_back else {
            continue;
        };
        let mut value = cell.lock().clone();
        match shape {
            Shape::Elements(class) => {
                let width = c_class_width(class).unwrap_or(1) as usize;
                let elements: Vec<Value> = buffer
                    .bytes
                    .chunks_exact(width)
                    .map(|chunk| scalar_value(class, read_class(chunk, class)))
                    .collect();
                let len = elements.len();
                crate::vm::overwrite_range(&mut value, 0, len, &Value::Array(Arc::new(elements)))?;
            }
            Shape::Leaves(leaves) => {
                for leaf in &leaves {
                    let word = read_class(&buffer.bytes[leaf.offset..], leaf.class);
                    set_leaf(&mut value, &leaf.steps, scalar_value(leaf.class, word))?;
                }
            }
        }
        *cell.lock() = value;
    }
    Ok(())
}

fn unknown_class(symbol: &str, class: char) -> RuntimeError {
    RuntimeError::Type(format!(
        "foreign function `{symbol}`: unknown class `{class}`"
    ))
}

/// The 64-bit word a scalar of `class` crosses in: an integer's bits, a
/// `double`'s bits, or a `float`'s bits in the low half.
fn scalar_word(symbol: &str, class: char, value: &Value) -> RuntimeResult<u64> {
    Ok(match (class, value) {
        ('f', Value::Float(f)) => u64::from((*f as f32).to_bits()),
        ('d', Value::Float(f)) => f.to_bits(),
        (_, Value::Int(n)) => *n as u64,
        (_, Value::Uint(n)) => *n,
        (_, Value::Bool(b)) => u64::from(*b),
        (_, Value::Char(c)) => u64::from(u32::from(*c)),
        (_, other) => {
            return Err(RuntimeError::Type(format!(
                "foreign function `{symbol}`: `{other}` is not a `{class}` argument"
            )));
        }
    })
}

/// The value a word of `class` reads back as.
fn scalar_value(class: char, word: u64) -> Value {
    match class {
        'B' => Value::Bool(word & 0xff != 0),
        'f' => Value::Float(f64::from(f32::from_bits(word as u32))),
        'd' => Value::Float(f64::from_bits(word)),
        _ => Value::Int(word as i64),
    }
}

fn write_class(bytes: &mut [u8], class: char, word: u64) {
    let width = c_class_width(class).unwrap_or(8) as usize;
    bytes[..width].copy_from_slice(&word.to_le_bytes()[..width]);
}

/// The word at the front of `bytes` for `class`, sign-extended for a signed
/// integer class.
fn read_class(bytes: &[u8], class: char) -> u64 {
    let width = c_class_width(class).unwrap_or(8) as usize;
    let mut word = [0u8; 8];
    word[..width].copy_from_slice(&bytes[..width]);
    if c_class_signed(class) && word[width - 1] & 0x80 != 0 {
        word[width..].fill(0xff);
    }
    u64::from_le_bytes(word)
}

fn pack_elements(symbol: &str, class: char, value: &Value) -> RuntimeResult<Vec<u8>> {
    if matches!(class, 'C' | 'c') {
        return Ok(value.bytes_or_empty());
    }
    let len = crate::vm::range_indexable_len(value).ok_or_else(|| {
        RuntimeError::Type(format!(
            "foreign function `{symbol}`: `{value}` is not a sequence"
        ))
    })?;
    let width = c_class_width(class).unwrap_or(8) as usize;
    let mut bytes = vec![0u8; len * width];
    for (i, chunk) in bytes.chunks_exact_mut(width).enumerate() {
        let element = crate::vm::index_get(value, &Value::Int(i as i64))?;
        write_class(chunk, class, scalar_word(symbol, class, &element)?);
    }
    Ok(bytes)
}

fn leaf_value(value: &Value, steps: &[Step]) -> RuntimeResult<Value> {
    let Some((step, rest)) = steps.split_first() else {
        return Ok(value.clone());
    };
    let inner = match (step, value) {
        (Step::Field(index), Value::Struct(inner)) => inner
            .fields
            .get(*index)
            .map(|(_, field)| field.clone())
            .ok_or_else(|| RuntimeError::Type("foreign struct field out of range".to_string()))?,
        (Step::Index(index), sequence) => crate::vm::index_get(sequence, &Value::Int(*index))?,
        (Step::Field(_), other) => {
            return Err(RuntimeError::Type(format!("`{other}` is not a struct")));
        }
    };
    leaf_value(&inner, rest)
}

fn set_leaf(value: &mut Value, steps: &[Step], scalar: Value) -> RuntimeResult<()> {
    let Some((step, rest)) = steps.split_first() else {
        *value = scalar;
        return Ok(());
    };
    match (step, value) {
        (Step::Field(index), Value::Struct(inner)) => {
            let inner = Arc::make_mut(inner);
            let Some((_, field)) = inner.fields.get_mut(*index) else {
                return Err(RuntimeError::Type(
                    "foreign struct field out of range".to_string(),
                ));
            };
            set_leaf(field, rest, scalar)
        }
        (Step::Index(index), sequence) => {
            let mut element = crate::vm::index_get(sequence, &Value::Int(*index))?;
            set_leaf(&mut element, rest, scalar)?;
            let at = usize::try_from(*index).unwrap_or(0);
            crate::vm::overwrite_range(sequence, at, at + 1, &Value::Array(Arc::new(vec![element])))
        }
        (Step::Field(_), other) => Err(RuntimeError::Type(format!("`{other}` is not a struct"))),
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod platform {
    use gossamer_abi::c_aggregate::{CAbi, CArg, CLayout, CallPlan, plan_call};
    use gossamer_codegen_cranelift::ffi_trampoline::{
        FfiKind, StubParam, StubRet, struct_trampoline, trampoline,
    };
    use gossamer_mir::ForeignParam;
    use gossamer_runtime::c_abi::ffi::{ffi_leave_capture, gos_rt_ffi_enter, resolve_symbol};

    use crate::value::{RuntimeError, RuntimeResult};

    /// The host's plan for a signature that moves a struct by value.
    pub(super) fn plan(
        params: &[ForeignParam],
        ret_struct: Option<&CLayout>,
        ret: char,
    ) -> RuntimeResult<CallPlan> {
        let abi = CAbi::host().ok_or_else(|| {
            RuntimeError::Panic(
                "a struct passed by value has no lowering on this platform".to_string(),
            )
        })?;
        let args: Vec<CArg> = params
            .iter()
            .map(|param| match param {
                ForeignParam::Scalar(class) => CArg::Scalar(*class),
                ForeignParam::Slice { .. } | ForeignParam::Struct { .. } => CArg::Scalar('L'),
                ForeignParam::ByValue(layout) => CArg::Aggregate(layout.clone()),
            })
            .collect();
        let ret = match (ret, ret_struct) {
            ('v', _) => None,
            (_, Some(layout)) => Some(CArg::Aggregate(layout.clone())),
            (class, None) => Some(CArg::Scalar(class)),
        };
        Ok(plan_call(abi, &args, ret.as_ref()))
    }

    pub(super) fn scalar_param(kind: FfiKind) -> StubParam {
        StubParam::Scalar(kind)
    }

    /// The stub parameter for struct argument `index` under `plan`.
    pub(super) fn struct_param(plan: Option<&CallPlan>, index: usize) -> RuntimeResult<StubParam> {
        plan.and_then(|plan| plan.params.get(index))
            .map(|arg| StubParam::Struct(arg.clone()))
            .ok_or_else(|| RuntimeError::Panic("a struct argument has no call plan".to_string()))
    }

    pub(super) fn invoke_struct(
        target: usize,
        params: &[StubParam],
        plan: &CallPlan,
        ret: char,
        words: &[u64],
    ) -> RuntimeResult<u64> {
        let stub_ret = if ret == 's' {
            StubRet::Struct(plan.ret.clone())
        } else {
            StubRet::Scalar(kind(ret).ok_or_else(|| {
                RuntimeError::Type(format!("foreign call: unknown result class `{ret}`"))
            })?)
        };
        let routine = struct_trampoline(params, &stub_ret)
            .map_err(|e| RuntimeError::Panic(format!("foreign call trampoline: {e}")))?;
        gos_rt_ffi_enter();
        // SAFETY: the routine was generated for exactly these parameters and
        // this result; `words` holds one word per parameter (a struct's being
        // the address of its bytes) and, for a struct result, the address of
        // its buffer, each outliving the call.
        #[allow(unsafe_code)]
        let result = unsafe { routine(target as *const u8, words.as_ptr()) };
        ffi_leave_capture();
        Ok(result)
    }

    pub(super) fn resolve(symbol: &str, libraries: &[String]) -> Option<usize> {
        resolve_symbol(symbol, libraries).map(|addr| addr as usize)
    }

    pub(super) fn kind(class: char) -> Option<FfiKind> {
        Some(match class {
            'c' => FfiKind::I8,
            'C' | 'B' => FfiKind::U8,
            'h' => FfiKind::I16,
            'H' => FfiKind::U16,
            'i' => FfiKind::I32,
            'I' => FfiKind::U32,
            'l' => FfiKind::I64,
            'L' => FfiKind::U64,
            'f' => FfiKind::F32,
            'd' => FfiKind::F64,
            'v' => FfiKind::Void,
            _ => return None,
        })
    }

    pub(super) fn pointer_kind() -> FfiKind {
        FfiKind::Ptr
    }

    pub(super) fn buffer_address(bytes: &mut Vec<u8>) -> u64 {
        if bytes.is_empty() {
            std::ptr::NonNull::<u8>::dangling().as_ptr() as u64
        } else {
            bytes.as_mut_ptr() as u64
        }
    }

    pub(super) fn invoke(
        target: usize,
        params: &[FfiKind],
        ret: FfiKind,
        words: &[u64],
    ) -> RuntimeResult<u64> {
        let routine = trampoline(params, ret)
            .map_err(|e| RuntimeError::Panic(format!("foreign call trampoline: {e}")))?;
        gos_rt_ffi_enter();
        // SAFETY: the routine was generated for exactly these parameter
        // kinds; `words` holds one word per parameter, and every pointer word
        // addresses a buffer that outlives the call. What the C function
        // itself does is the program's `unsafe` block's contract.
        #[allow(unsafe_code)]
        let result = unsafe { routine(target as *const u8, words.as_ptr()) };
        ffi_leave_capture();
        Ok(result)
    }
}

// wasm32 has no foreign functions to call: there is no dynamic loader.
#[cfg(target_arch = "wasm32")]
mod platform {
    use crate::value::{RuntimeError, RuntimeResult};

    pub(super) fn resolve(_symbol: &str, _libraries: &[String]) -> Option<usize> {
        None
    }

    pub(super) fn kind(class: char) -> Option<char> {
        Some(class)
    }

    pub(super) fn pointer_kind() -> char {
        'p'
    }

    pub(super) fn buffer_address(_bytes: &mut Vec<u8>) -> u64 {
        0
    }

    pub(super) fn invoke(_: usize, _: &[char], _: char, _: &[u64]) -> RuntimeResult<u64> {
        Err(unavailable())
    }

    fn unavailable() -> RuntimeError {
        RuntimeError::Panic("foreign functions are not available on wasm32".to_string())
    }

    pub(super) fn plan(
        _: &[gossamer_mir::ForeignParam],
        _: Option<&gossamer_abi::c_aggregate::CLayout>,
        _: char,
    ) -> RuntimeResult<gossamer_abi::c_aggregate::CallPlan> {
        Err(unavailable())
    }

    pub(super) fn scalar_param(kind: char) -> char {
        kind
    }

    pub(super) fn struct_param(
        _: Option<&gossamer_abi::c_aggregate::CallPlan>,
        _: usize,
    ) -> RuntimeResult<char> {
        Err(unavailable())
    }

    pub(super) fn invoke_struct(
        _: usize,
        _: &[char],
        _: &gossamer_abi::c_aggregate::CallPlan,
        _: char,
        _: &[u64],
    ) -> RuntimeResult<u64> {
        Err(unavailable())
    }
}

/// Callbacks on the bytecode tier: a function the program passes as a C
/// function pointer reaches native code as a reverse trampoline that calls
/// [`callbacks::entry`] with the callback's slot, which re-enters the
/// interpreter running the foreign call on this thread.
pub(crate) mod callbacks {
    use std::cell::RefCell;

    use parking_lot::Mutex;

    use crate::value::{NativeDispatch, RuntimeError, RuntimeResult, Value};

    thread_local! {
        /// The interpreters running foreign calls on this thread, innermost
        /// last: the one a callback re-enters.
        // Each entry addresses a `&mut dyn NativeDispatch` on a `with_active`
        // frame, erased to a thin pointer so no lifetime is stored.
        static ACTIVE: RefCell<Vec<*mut ()>> =
            const { RefCell::new(Vec::new()) };
        /// A fault a callback raised, reported when the foreign call that
        /// ran it returns.
        static PENDING: RefCell<Option<RuntimeError>> = const { RefCell::new(None) };
    }

    /// A registered callback: its key, the adapter, the program's function
    /// name for reports, and how to run it on a thread the program did not
    /// start.
    struct Slot {
        key: String,
        adapter: Value,
        name: String,
        runner: Option<crate::value::ForeignThreadRunner>,
    }

    /// Registered callbacks by slot.
    static SLOTS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

    /// Runs `call` with `dispatch` as the interpreter callbacks on this
    /// thread re-enter, then reports a fault one raised.
    pub(crate) fn with_active(
        dispatch: &mut dyn NativeDispatch,
        call: impl FnOnce() -> RuntimeResult<Value>,
    ) -> RuntimeResult<Value> {
        let mut dispatch = dispatch;
        // The entry is read only by callbacks on this thread while `call`
        // runs, during which `dispatch` lives on this frame and is otherwise
        // unused; it is removed before this function returns.
        let raw = (&raw mut dispatch).cast::<()>();
        ACTIVE.with(|active| active.borrow_mut().push(raw));
        let result = call();
        ACTIVE.with(|active| active.borrow_mut().pop());
        if let Some(fault) = PENDING.with(|pending| pending.borrow_mut().take()) {
            return Err(fault);
        }
        result
    }

    /// The slot of the callback that runs `adapter` for the function
    /// `name`, registering it on first use.
    fn slot(
        adapter: &Value,
        key: &str,
        name: &str,
        runner: Option<crate::value::ForeignThreadRunner>,
    ) -> u64 {
        let mut slots = SLOTS.lock();
        if let Some(index) = slots.iter().position(|slot| slot.key == key) {
            return index as u64;
        }
        slots.push(Slot {
            key: key.to_string(),
            adapter: adapter.clone(),
            name: name.to_string(),
            runner,
        });
        (slots.len() - 1) as u64
    }

    /// `__gos_ffi_callback(adapter, signature, name)`: the address of a
    /// C-ABI entry that runs `adapter` on this interpreter.
    pub(crate) fn native_ffi_callback(
        dispatch: &mut dyn NativeDispatch,
        args: &[Value],
    ) -> RuntimeResult<Value> {
        let [adapter, Value::String(signature), Value::String(name)] = args else {
            return Err(RuntimeError::Type(
                "__gos_ffi_callback: an adapter, a signature, and a name".to_string(),
            ));
        };
        let key = format!("{name}:{signature}");
        let slot = slot(
            adapter,
            &key,
            name.as_str(),
            dispatch.foreign_thread_runner(),
        );
        platform::entry_address(signature.as_str(), slot).map(|addr| Value::Uint(addr as u64))
    }

    /// What a reverse trampoline calls: runs the adapter in `slot` over the
    /// argument words on the interpreter running this thread's foreign
    /// call, and answers its result word.
    // wasm32 builds no trampoline, so nothing there calls it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) extern "C" fn entry(slot: u64, words: *const u64) -> u64 {
        let (adapter, name, runner) = {
            let slots = SLOTS.lock();
            let Some(registered) = slots.get(slot as usize) else {
                return 0;
            };
            (
                registered.adapter.clone(),
                registered.name.clone(),
                registered.runner.clone(),
            )
        };
        let Some(dispatch) = ACTIVE.with(|active| active.borrow().last().copied()) else {
            return on_foreign_thread(runner.as_ref(), &adapter, &name, words);
        };
        if PENDING.with(|pending| pending.borrow().is_some()) {
            return 0;
        }
        let run = std::panic::AssertUnwindSafe(|| {
            let dispatch = dispatch.cast::<&mut dyn NativeDispatch>();
            // SAFETY: `dispatch` addresses the interpreter whose foreign call
            // is running on this thread (`with_active`), suspended in that call.
            #[allow(unsafe_code)]
            unsafe {
                (*dispatch).call_value(&adapter, vec![Value::Int(words as i64)])
            }
        });
        let outcome = std::panic::catch_unwind(run).unwrap_or_else(|_| {
            Err(RuntimeError::Panic(format!(
                "the callback `{name}` failed inside the interpreter"
            )))
        });
        match outcome {
            Ok(Value::Int(word)) => word as u64,
            Ok(Value::Uint(word)) => word,
            Ok(_) => 0,
            Err(fault) => {
                PENDING.with(|pending| *pending.borrow_mut() = Some(fault));
                0
            }
        }
    }

    /// Runs a callback native code invoked on a thread outside the program's
    /// foreign calls, on that thread with an interpreter of its own; a fault
    /// there has no foreign call to resume in, so it ends the program with
    /// the report a compiled program prints.
    #[cfg(not(target_arch = "wasm32"))]
    fn on_foreign_thread(
        runner: Option<&crate::value::ForeignThreadRunner>,
        adapter: &Value,
        name: &str,
        words: *const u64,
    ) -> u64 {
        let Some(runner) = runner else {
            return 0;
        };
        gossamer_runtime::c_abi::ffi::attach_foreign_thread();
        let run = std::panic::AssertUnwindSafe(|| runner(adapter, vec![Value::Int(words as i64)]));
        let outcome = std::panic::catch_unwind(run).unwrap_or_else(|_| {
            Err(RuntimeError::Panic(format!(
                "the callback `{name}` failed inside the interpreter"
            )))
        });
        match outcome {
            Ok(Value::Int(word)) => word as u64,
            Ok(Value::Uint(word)) => word,
            Ok(_) => 0,
            Err(fault) => {
                use std::io::Write as _;
                crate::flush_runtime_stdout();
                crate::run_exit_hooks();
                let mut err = std::io::stderr();
                let _ = writeln!(err, "{fault}");
                let _ = err.flush();
                std::process::exit(101);
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    mod platform {
        use gossamer_codegen_cranelift::ffi_trampoline::{FfiKind, reverse_trampoline};

        use crate::value::{RuntimeError, RuntimeResult};

        fn kind(class: char) -> Option<FfiKind> {
            Some(match class {
                'c' => FfiKind::I8,
                'C' | 'B' => FfiKind::U8,
                'h' => FfiKind::I16,
                'H' => FfiKind::U16,
                'i' => FfiKind::I32,
                'I' => FfiKind::U32,
                'l' => FfiKind::I64,
                'L' => FfiKind::U64,
                'f' => FfiKind::F32,
                'd' => FfiKind::F64,
                'v' => FfiKind::Void,
                _ => return None,
            })
        }

        pub(super) fn entry_address(signature: &str, slot: u64) -> RuntimeResult<usize> {
            let malformed =
                || RuntimeError::Type(format!("callback: malformed signature `{signature}`"));
            let (params, ret) = signature.split_once('>').ok_or_else(malformed)?;
            if signature.contains("s(") {
                return struct_entry_address(params, ret, slot).ok_or_else(malformed)?;
            }
            let params = params
                .chars()
                .map(kind)
                .collect::<Option<Vec<_>>>()
                .ok_or_else(malformed)?;
            let ret = ret.chars().next().and_then(kind).ok_or_else(malformed)?;
            reverse_trampoline(&params, ret, super::entry, slot)
                .map_err(|e| RuntimeError::Panic(format!("callback trampoline: {e}")))
        }

        /// The entry for a callback that takes or answers a struct by value,
        /// planned for the host's calling convention; `None` for a malformed
        /// signature.
        fn struct_entry_address(
            params: &str,
            ret: &str,
            slot: u64,
        ) -> Option<RuntimeResult<usize>> {
            use gossamer_abi::c_aggregate::{CAbi, CArg, CLayout, plan_call};
            use gossamer_codegen_cranelift::ffi_trampoline::{
                StubParam, StubRet, reverse_struct_trampoline,
            };
            use gossamer_mir::ForeignParam;

            let params = gossamer_mir::foreign_params(params)?;
            let ret_struct = match ret.strip_prefix("s(") {
                Some(inner) => Some(CLayout::parse(inner.strip_suffix(')')?)?),
                None => None,
            };
            let Some(abi) = CAbi::host() else {
                return Some(Err(RuntimeError::Panic(
                    "a struct passed by value has no lowering on this platform".to_string(),
                )));
            };
            let mut args = Vec::with_capacity(params.len());
            let mut sizes = Vec::with_capacity(params.len() + 1);
            for param in &params {
                match param {
                    ForeignParam::Scalar(class) => {
                        args.push(CArg::Scalar(*class));
                        sizes.push(8);
                    }
                    ForeignParam::ByValue(layout) => {
                        sizes.push(layout.size);
                        args.push(CArg::Aggregate(layout.clone()));
                    }
                    ForeignParam::Slice { .. } | ForeignParam::Struct { .. } => return None,
                }
            }
            let ret_arg = match (&ret_struct, ret.chars().next()?) {
                (Some(layout), _) => {
                    sizes.push(layout.size);
                    Some(CArg::Aggregate(layout.clone()))
                }
                (None, 'v') => None,
                (None, class) => Some(CArg::Scalar(class)),
            };
            let plan = plan_call(abi, &args, ret_arg.as_ref());
            let mut stub_params = Vec::with_capacity(params.len());
            for (param, arg_plan) in params.iter().zip(&plan.params) {
                stub_params.push(match param {
                    ForeignParam::Scalar(class) => StubParam::Scalar(kind(*class)?),
                    _ => StubParam::Struct(arg_plan.clone()),
                });
            }
            let stub_ret = if ret_struct.is_some() {
                StubRet::Struct(plan.ret.clone())
            } else {
                StubRet::Scalar(kind(ret.chars().next()?)?)
            };
            Some(
                reverse_struct_trampoline(&stub_params, &stub_ret, &sizes, super::entry, slot)
                    .map_err(|e| RuntimeError::Panic(format!("callback trampoline: {e}"))),
            )
        }
    }

    // wasm32 has no foreign functions, so nothing calls back.
    #[cfg(target_arch = "wasm32")]
    mod platform {
        use crate::value::{RuntimeError, RuntimeResult};

        pub(super) fn entry_address(_signature: &str, _slot: u64) -> RuntimeResult<usize> {
            Err(RuntimeError::Panic(
                "foreign functions are not available on wasm32".to_string(),
            ))
        }
    }
}

/// `ffi::Handle` on the bytecode tier: values native code holds as an
/// integer, each in the one-element cell `Handle::new` built.
pub(crate) mod handles {
    use std::collections::HashMap;

    use parking_lot::Mutex;

    use crate::value::{RuntimeError, RuntimeResult, Value};

    static CELLS: Mutex<Option<(u64, HashMap<u64, Value>)>> = Mutex::new(None);

    fn with<R>(f: impl FnOnce(&mut (u64, HashMap<u64, Value>)) -> R) -> R {
        let mut cells = CELLS.lock();
        f(cells.get_or_insert_with(|| (1, HashMap::new())))
    }

    fn id_of(value: &Value) -> RuntimeResult<u64> {
        match value {
            Value::Int(n) => Ok(*n as u64),
            Value::Uint(n) => Ok(*n),
            other => Err(RuntimeError::Type(format!(
                "a handle id is an integer, not `{other}`"
            ))),
        }
    }

    fn missing(id: u64) -> RuntimeError {
        RuntimeError::Foreign {
            code: "GX0014",
            message: gossamer_runtime::c_abi::ffi::missing_handle_message(id),
        }
    }

    /// `__gos_ffi_handle_pin(cell)`.
    // Every builtin answers the shared fallible builtin signature.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn builtin_pin(args: &[Value]) -> RuntimeResult<Value> {
        let cell = args.first().cloned().unwrap_or(Value::Unit);
        Ok(Value::Uint(with(|(next, cells)| {
            let id = *next;
            *next += 1;
            cells.insert(id, cell);
            id
        })))
    }

    /// `__gos_ffi_handle_cell(id)`.
    pub(crate) fn builtin_cell(args: &[Value]) -> RuntimeResult<Value> {
        let id = id_of(args.first().unwrap_or(&Value::Unit))?;
        with(|(_, cells)| cells.get(&id).cloned()).ok_or_else(|| missing(id))
    }

    /// `__gos_ffi_handle_store(id, cell)`.
    pub(crate) fn builtin_store(args: &[Value]) -> RuntimeResult<Value> {
        let id = id_of(args.first().unwrap_or(&Value::Unit))?;
        let cell = args.get(1).cloned().unwrap_or(Value::Unit);
        with(|(_, cells)| match cells.get_mut(&id) {
            Some(slot) => {
                *slot = cell;
                Ok(Value::Unit)
            }
            None => Err(missing(id)),
        })
    }

    /// `__gos_ffi_handle_release(id)`.
    pub(crate) fn builtin_release(args: &[Value]) -> RuntimeResult<Value> {
        let id = id_of(args.first().unwrap_or(&Value::Unit))?;
        with(|(_, cells)| cells.remove(&id))
            .map(|_| Value::Unit)
            .ok_or_else(|| missing(id))
    }
}

/// `__gos_ffi_symbol(symbol, library)`: the address of a C global, looked up
/// in `library` and then the process, as a foreign function's is.
pub(crate) fn builtin_ffi_symbol(args: &[Value]) -> RuntimeResult<Value> {
    let [Value::String(symbol), Value::String(library)] = args else {
        return Err(RuntimeError::Type(
            "__gos_ffi_symbol: a symbol and a library".to_string(),
        ));
    };
    resolve(symbol.as_str(), library.as_str()).map(|addr| Value::Uint(addr as u64))
}

/// `__gos_ffi_null_result(symbol)`: GX0013.
pub(crate) fn builtin_ffi_null_result(args: &[Value]) -> RuntimeResult<Value> {
    let symbol = match args.first() {
        Some(Value::String(symbol)) => symbol.to_string(),
        _ => String::new(),
    };
    Err(RuntimeError::Foreign {
        code: "GX0013",
        message: gossamer_runtime::c_abi::ffi::null_result_message(&symbol),
    })
}

/// Foreign memory access on the bytecode tier: the runtime's own loads,
/// stores, atomics, and `ffi::View` bounds checks, which the compiled tiers
/// call directly.
pub(crate) mod memory {
    use crate::value::{RuntimeError, RuntimeResult, Value};

    fn int(args: &[Value], index: usize) -> RuntimeResult<i64> {
        match args.get(index) {
            Some(Value::Int(n)) => Ok(*n),
            Some(Value::Uint(n)) => Ok(*n as i64),
            Some(Value::Bool(b)) => Ok(i64::from(*b)),
            Some(Value::Char(c)) => Ok(i64::from(u32::from(*c))),
            other => Err(RuntimeError::Type(format!(
                "foreign memory: argument {} is not an integer: {other:?}",
                index + 1
            ))),
        }
    }

    fn float(args: &[Value], index: usize) -> RuntimeResult<f64> {
        match args.get(index) {
            Some(Value::Float(f)) => Ok(*f),
            Some(Value::Int(n)) => Ok(*n as f64),
            other => Err(RuntimeError::Type(format!(
                "foreign memory: argument {} is not a float: {other:?}",
                index + 1
            ))),
        }
    }

    /// `__gos_ffi_load_int(address, class)`.
    pub(crate) fn builtin_load_int(args: &[Value]) -> RuntimeResult<Value> {
        let (addr, class) = (int(args, 0)? as u64, int(args, 1)?);
        // SAFETY: the program's `unsafe` block vouches for the address.
        #[allow(unsafe_code)]
        let value = unsafe { gossamer_runtime::c_abi::ffi::load_int(addr, class) };
        Ok(Value::Int(value))
    }

    /// `__gos_ffi_load_float(address, class)`.
    pub(crate) fn builtin_load_float(args: &[Value]) -> RuntimeResult<Value> {
        let (addr, class) = (int(args, 0)? as u64, int(args, 1)?);
        // SAFETY: the program's `unsafe` block vouches for the address.
        #[allow(unsafe_code)]
        let value = unsafe { gossamer_runtime::c_abi::ffi::load_float(addr, class) };
        Ok(Value::Float(value))
    }

    /// `__gos_ffi_store_int(address, class, value)`.
    pub(crate) fn builtin_store_int(args: &[Value]) -> RuntimeResult<Value> {
        let (addr, class, value) = (int(args, 0)? as u64, int(args, 1)?, int(args, 2)?);
        // SAFETY: the program's `unsafe` block vouches for the address.
        #[allow(unsafe_code)]
        unsafe {
            gossamer_runtime::c_abi::ffi::store_int(addr, class, value);
        }
        Ok(Value::Unit)
    }

    /// `__gos_ffi_store_float(address, class, value)`.
    pub(crate) fn builtin_store_float(args: &[Value]) -> RuntimeResult<Value> {
        let (addr, class, value) = (int(args, 0)? as u64, int(args, 1)?, float(args, 2)?);
        // SAFETY: the program's `unsafe` block vouches for the address.
        #[allow(unsafe_code)]
        unsafe {
            gossamer_runtime::c_abi::ffi::store_float(addr, class, value);
        }
        Ok(Value::Unit)
    }

    /// `__gos_ffi_view_check(index, len)`.
    pub(crate) fn builtin_view_check(args: &[Value]) -> RuntimeResult<Value> {
        let (index, len) = (int(args, 0)?, int(args, 1)?);
        if index < 0 || index >= len {
            return Err(RuntimeError::Panic(
                gossamer_runtime::c_abi::ffi::view_index_message(index, len),
            ));
        }
        Ok(Value::Unit)
    }

    /// `__gos_ffi_view_range_check(lo, hi, len)`.
    pub(crate) fn builtin_view_range_check(args: &[Value]) -> RuntimeResult<Value> {
        let (lo, hi, len) = (int(args, 0)?, int(args, 1)?, int(args, 2)?);
        if lo < 0 || lo > hi || hi > len {
            return Err(RuntimeError::Panic(
                gossamer_runtime::c_abi::ffi::view_range_message(lo, hi, len),
            ));
        }
        Ok(Value::Unit)
    }

    /// `__gos_ffi_view_len_check(len, found)`.
    pub(crate) fn builtin_view_len_check(args: &[Value]) -> RuntimeResult<Value> {
        let (len, found) = (int(args, 0)?, int(args, 1)?);
        if found != len {
            return Err(RuntimeError::Panic(
                gossamer_runtime::c_abi::ffi::view_len_message(len, found),
            ));
        }
        Ok(Value::Unit)
    }

    /// `__gos_ffi_atomic_rmw(address, op, width, value)`.
    pub(crate) fn builtin_atomic_rmw(args: &[Value]) -> RuntimeResult<Value> {
        let (addr, op, width, value) = (
            int(args, 0)? as u64,
            int(args, 1)?,
            int(args, 2)?,
            int(args, 3)?,
        );
        // SAFETY: the program's `unsafe` block vouches for the address.
        #[allow(unsafe_code)]
        let result = unsafe { gossamer_runtime::c_abi::ffi::atomic_rmw(addr, op, width, value) };
        result.map(Value::Int).map_err(RuntimeError::Panic)
    }

    /// `__gos_ffi_atomic_cas(address, width, expected, new)`.
    pub(crate) fn builtin_atomic_cas(args: &[Value]) -> RuntimeResult<Value> {
        let (addr, width, expected, new) = (
            int(args, 0)? as u64,
            int(args, 1)?,
            int(args, 2)?,
            int(args, 3)?,
        );
        // SAFETY: the program's `unsafe` block vouches for the address.
        #[allow(unsafe_code)]
        let result =
            unsafe { gossamer_runtime::c_abi::ffi::atomic_cas(addr, width, expected, new) };
        result.map(Value::Int).map_err(RuntimeError::Panic)
    }
}
