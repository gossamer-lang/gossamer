/// `__gos_par_run(len, mode, leaf)`: runs `leaf(lo, hi)` over the leaves of
/// `[0, len)`, on this goroutine and on helpers from the pool, and answers
/// every leaf's sequence concatenated in index order. The leaves are cut by
/// the same runtime rule the compiled tiers use, so a reduction's tree is the
/// same shape on every tier.
fn native_par_run(dispatch: &mut dyn NativeDispatch, args: &[Value]) -> RuntimeResult<Value> {
    use gossamer_runtime::c_abi::par::{Job, Leaves, Mode, arm_stats_report, note_submitted};

    arm_stats_report();
    let [len, mode, leaf] = args else {
        return Err(RuntimeError::Arity {
            expected: 3,
            found: args.len(),
        });
    };
    let len = value_to_int(len).unwrap_or(0);
    let mode = Mode::from_code(value_to_int(mode).unwrap_or(0));
    let workers = crate::vm::goroutine::parallel_width();
    let leaves = Leaves::new(len, mode, workers);
    if leaves.count() == 1 {
        let (lo, hi) = leaves.bounds(0);
        return dispatch.call_value(leaf, vec![Value::Int(lo), Value::Int(hi)]);
    }
    let job = Job::<Value, RuntimeError>::new(leaves);
    for _ in 0..leaves.helpers(workers) {
        let helper_job = Arc::clone(&job);
        let helper_leaf = leaf.clone();
        let queued = dispatch.spawn_task(Box::new(move |helper: &mut dyn NativeDispatch| {
            helper_job.help(|lo, hi| call_par_leaf(helper, &helper_leaf, lo, hi));
        }));
        if !queued {
            break;
        }
        note_submitted(1);
    }
    match job.run(|lo, hi| call_par_leaf(dispatch, leaf, lo, hi)) {
        Ok(parts) => Ok(concat_par_parts(parts)),
        Err((err, _)) => Err(err),
    }
}

/// Runs one leaf with any fault a compiled body raises inside it held as the
/// leaf's error rather than reported, so only the lowest failing leaf's is.
fn call_par_leaf(
    dispatch: &mut dyn NativeDispatch,
    leaf: &Value,
    lo: i64,
    hi: i64,
) -> RuntimeResult<Value> {
    gossamer_runtime::c_abi::par::run_deferred(|| {
        dispatch.call_value(leaf, vec![Value::Int(lo), Value::Int(hi)])
    })
    .unwrap_or_else(|fault| Err(RuntimeError::Panic(fault.text)))
}

/// Concatenates leaf sequences in order, keeping the storage shape of the
/// first non-empty one.
fn concat_par_parts(parts: Vec<Value>) -> Value {
    let base = parts
        .iter()
        .position(|part| array_as_values(part).is_some_and(|items| !items.is_empty()))
        .unwrap_or(0);
    match parts.get(base) {
        Some(Value::IntArray(_)) => {
            let mut out: Vec<i64> = Vec::new();
            for part in &parts {
                if let Value::IntArray(items) = part {
                    out.extend(items.iter().copied());
                } else if let Some(items) = array_as_values(part) {
                    out.extend(items.iter().filter_map(value_to_int));
                }
            }
            Value::IntArray(Arc::new(out))
        }
        Some(Value::FloatVec(_)) => {
            let mut out: Vec<f64> = Vec::new();
            for part in &parts {
                if let Value::FloatVec(items) = part {
                    out.extend(items.iter().copied());
                } else if let Some(items) = array_as_values(part) {
                    out.extend(items.iter().filter_map(|item| match item {
                        Value::Float(f) => Some(*f),
                        Value::Int(n) => Some(*n as f64),
                        _ => None,
                    }));
                }
            }
            Value::FloatVec(Arc::new(out))
        }
        Some(Value::ByteVec(_)) => {
            let mut out: Vec<u8> = Vec::new();
            for part in &parts {
                if let Value::ByteVec(items) = part {
                    out.extend(items.iter().copied());
                } else if let Some(items) = array_as_values(part) {
                    out.extend(
                        items
                            .iter()
                            .filter_map(value_to_int)
                            .filter_map(|n| u8::try_from(n).ok()),
                    );
                }
            }
            Value::ByteVec(Arc::new(out))
        }
        _ => {
            let mut out: Vec<Value> = Vec::new();
            for part in parts {
                match part {
                    Value::Array(items) => {
                        out.extend(Arc::try_unwrap(items).unwrap_or_else(|shared| (*shared).clone()));
                    }
                    other => out.extend(array_as_values(&other).unwrap_or_default()),
                }
            }
            Value::Array(Arc::new(out))
        }
    }
}

/// `__gos_windows_disjoint(len, a_lo, a_hi, a_inclusive, b_lo, b_hi,
/// b_inclusive)`: panics when two mutable windows of one sequence passed to
/// one call overlap, with the text the compiled tiers raise.
fn builtin_windows_disjoint(args: &[Value]) -> RuntimeResult<Value> {
    let ints: Vec<i64> = args
        .iter()
        .map(crate::vm::index_value)
        .collect::<RuntimeResult<_>>()?;
    let [len, a_lo, a_hi, a_inclusive, b_lo, b_hi, b_inclusive] = ints[..] else {
        return Err(RuntimeError::Arity {
            expected: 7,
            found: args.len(),
        });
    };
    match gossamer_runtime::c_abi::vec::window_overlap_message(
        len,
        (a_lo, a_hi, a_inclusive),
        (b_lo, b_hi, b_inclusive),
    ) {
        Some(message) => Err(RuntimeError::Panic(message)),
        None => Ok(Value::Unit),
    }
}

/// `__gos_par_chunks(window, size, f)`: hands each `size`-element chunk of the
/// window to `f` with its index, in index order, and writes each chunk back.
/// The callback is pure apart from its chunk, so running the chunks in order
/// answers what the compiled tiers' pool does, and the first chunk to fault
/// is the lowest-indexed one, which is the fault they raise.
fn native_par_chunks(dispatch: &mut dyn NativeDispatch, args: &[Value]) -> RuntimeResult<Value> {
    let [window, size, callback] = args else {
        return Err(RuntimeError::Arity {
            expected: 3,
            found: args.len(),
        });
    };
    let Value::MutCell(window) = window else {
        return Err(RuntimeError::Type(
            "par_chunks_mut: the sequence must be passed by `&mut`".to_string(),
        ));
    };
    let size = crate::vm::index_value(size)?;
    if size <= 0 {
        return Err(RuntimeError::Panic(format!(
            "par_chunks_mut: chunk size must be positive, got {size}"
        )));
    }
    let mut whole = window.lock().clone();
    let Some(len) = crate::vm::range_indexable_len(&whole) else {
        return Err(RuntimeError::Type(format!(
            "par_chunks_mut: value of kind `{whole}` is not a sequence"
        )));
    };
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    for (index, lo) in (0..len).step_by(size).enumerate() {
        let hi = lo.saturating_add(size).min(len);
        let chunk = crate::vm::index_range_get(&whole, lo as i64, hi as i64, false, false, false)?;
        let cell = Arc::new(crate::value::ThreadConfinedCell::new(chunk));
        dispatch.call_value(
            callback,
            vec![Value::Int(index as i64), Value::MutCell(Arc::clone(&cell))],
        )?;
        let written = cell.lock().clone();
        crate::vm::overwrite_range(&mut whole, lo, hi, &written)?;
    }
    *window.lock() = whole;
    Ok(Value::Unit)
}
