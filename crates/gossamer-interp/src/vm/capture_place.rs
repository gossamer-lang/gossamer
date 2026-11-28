//! Reads and writes of a place inside a captured binding's cell, the value a
//! `&mut` argument naming such a place crosses as.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::value::{CaptureStep, Value};
use crate::vm::{RuntimeError, RuntimeResult};

/// The cell a capture-cell instruction's operand names and the steps from
/// its value to the binding: none for the cell of a binding, the place's
/// steps for a [`Value::CapturePlace`].
pub(crate) fn target(value: &Value) -> Option<(&Arc<Mutex<Value>>, &[CaptureStep])> {
    match value {
        Value::CaptureCell(cell) => Some((cell, &[])),
        Value::CapturePlace(place) => Some((&place.cell, &place.steps)),
        _ => None,
    }
}

/// A copy of the value at `steps` under `root`.
pub(crate) fn read(root: &Value, steps: &[CaptureStep]) -> RuntimeResult<Value> {
    let mut current = root.clone();
    for step in steps {
        current = step_get(&current, step)?;
    }
    Ok(current)
}

/// The slot at `steps` under `root`, each level made unique on the way down
/// so a write through it lands where the value lives.
pub(crate) fn slot<'a>(root: &'a mut Value, steps: &[CaptureStep]) -> RuntimeResult<&'a mut Value> {
    let mut current = root;
    for step in steps {
        current = step_mut(current, step)?.ok_or_else(|| unreachable_step(step))?;
    }
    Ok(current)
}

/// Moves the value at `steps` out of `root`, leaving `()` in its slot so the
/// value keeps a refcount of one while an instruction works on it. A scalar
/// held in packed storage has no slot of its own and is copied.
pub(crate) fn take(root: &mut Value, steps: &[CaptureStep]) -> RuntimeResult<Value> {
    let Some((last, parents)) = steps.split_last() else {
        return Ok(std::mem::replace(root, Value::Unit));
    };
    let parent = slot(root, parents)?;
    match step_mut(parent, last)? {
        Some(slot) => Ok(std::mem::replace(slot, Value::Unit)),
        None => step_get(parent, last),
    }
}

/// Stores `value` at `steps` under `root`.
pub(crate) fn put(root: &mut Value, steps: &[CaptureStep], value: Value) -> RuntimeResult<()> {
    let Some((last, parents)) = steps.split_last() else {
        *root = value;
        return Ok(());
    };
    let parent = slot(root, parents)?;
    match step_mut(parent, last)? {
        Some(slot) => {
            *slot = value;
            Ok(())
        }
        None => match last {
            CaptureStep::Index(index) => {
                super::run::index_set_value(parent, index_i64(*index), value)
            }
            CaptureStep::Field(name) => super::field_set(parent, name, value),
            CaptureStep::Tuple(_) => Err(unreachable_step(last)),
        },
    }
}

fn step_get(value: &Value, step: &CaptureStep) -> RuntimeResult<Value> {
    match step {
        CaptureStep::Field(name) => super::field_get(value, name),
        CaptureStep::Index(index) => super::index_get(value, &Value::Int(index_i64(*index))),
        CaptureStep::Tuple(n) => match value {
            Value::Tuple(items) | Value::Array(items) => {
                items.get(*n).cloned().ok_or_else(|| unreachable_step(step))
            }
            Value::Struct(inner) => inner
                .fields
                .iter()
                .nth(*n)
                .map(|(_, field)| field.clone())
                .ok_or_else(|| unreachable_step(step)),
            _ => Err(unreachable_step(step)),
        },
    }
}

/// The boxed slot `step` names under `value`, made unique; `None` where the
/// slot lies in packed storage.
fn step_mut<'a>(value: &'a mut Value, step: &CaptureStep) -> RuntimeResult<Option<&'a mut Value>> {
    Ok(match (step, value) {
        (CaptureStep::Field(name), Value::Struct(inner)) => Arc::make_mut(inner)
            .fields
            .iter_mut()
            .find(|(ident, _)| ***ident == **name)
            .map(|(_, slot)| slot),
        (
            CaptureStep::Index(n) | CaptureStep::Tuple(n),
            Value::Array(items) | Value::Tuple(items),
        ) => {
            let len = items.len();
            let Some(slot) = Arc::make_mut(items).get_mut(*n) else {
                return Err(super::index_oob_panic(index_i64(*n), len));
            };
            Some(slot)
        }
        (CaptureStep::Tuple(n), Value::Struct(inner)) => Arc::make_mut(inner)
            .fields
            .iter_mut()
            .nth(*n)
            .map(|(_, slot)| slot),
        _ => None,
    })
}

fn index_i64(index: usize) -> i64 {
    i64::try_from(index).unwrap_or(i64::MAX)
}

fn unreachable_step(step: &CaptureStep) -> RuntimeError {
    RuntimeError::Type(format!("a captured place has no slot at {step:?}"))
}
