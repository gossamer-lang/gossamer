use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use gossamer_abi::static_storage::{StaticClass, StaticStorage};

use crate::value::Value;

/// A `static mut`'s storage, shared by every goroutine's VM. A scalar static
/// lives in words compiled code addresses directly, so both tiers read and
/// write one value; any other static holds a VM value, and its words (handed
/// to every JIT artifact) are compiled code's own.
pub(crate) struct StaticCell {
    /// The MIR symbol compiled code names the static by.
    symbol: Option<String>,
    class: Option<StaticClass>,
    value: parking_lot::Mutex<Value>,
    storage: OnceLock<Arc<StaticStorage>>,
    written: AtomicBool,
}

impl std::fmt::Debug for StaticCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticCell")
            .field("symbol", &self.symbol)
            .field("value", &self.get())
            .finish_non_exhaustive()
    }
}

impl StaticCell {
    pub(crate) fn new(
        value: Value,
        def: Option<gossamer_resolve::DefId>,
        class: Option<StaticClass>,
    ) -> Self {
        let storage = OnceLock::new();
        if let Some(class) = class {
            let _ = storage.set(Arc::new(StaticStorage::scalar(
                class,
                encode(class, &value),
            )));
        }
        Self {
            symbol: def.map(|def| format!("gos_static_{}", def.local)),
            class,
            value: parking_lot::Mutex::new(value),
            storage,
            written: AtomicBool::new(false),
        }
    }

    pub(crate) fn get(&self) -> Value {
        match (self.class, self.storage.get()) {
            (Some(class), Some(storage)) => decode(class, storage.load(class)),
            _ => self.value.lock().clone(),
        }
    }

    pub(crate) fn set(&self, next: Value) {
        self.written.store(true, Ordering::Relaxed);
        match (self.class, self.storage.get()) {
            (Some(class), Some(storage)) => storage.store(class, encode(class, &next)),
            _ => *self.value.lock() = next,
        }
    }

    pub(crate) fn symbol(&self) -> Option<&str> {
        self.symbol.as_deref()
    }

    /// Whether interpreted code has stored into the static.
    pub(crate) fn written(&self) -> bool {
        self.written.load(Ordering::Relaxed)
    }

    /// The words compiled code uses for this static: a scalar's own storage,
    /// or `default` the first time any artifact asks.
    pub(crate) fn storage(&self, default: StaticStorage) -> Arc<StaticStorage> {
        Arc::clone(self.storage.get_or_init(|| Arc::new(default)))
    }
}

fn encode(class: StaticClass, value: &Value) -> u64 {
    match (class, value) {
        (StaticClass::Int, Value::Int(n)) => *n as u64,
        (StaticClass::Float, Value::Float(f)) => f.to_bits(),
        (StaticClass::Bool, Value::Bool(b)) => u64::from(*b),
        (StaticClass::Char, Value::Char(c)) => u64::from(u32::from(*c)),
        _ => unreachable!("a {class:?} static holds {value:?}"),
    }
}

fn decode(class: StaticClass, bits: u64) -> Value {
    match class {
        StaticClass::Int => Value::Int(bits as i64),
        StaticClass::Float => Value::Float(f64::from_bits(bits)),
        StaticClass::Bool => Value::Bool(bits != 0),
        // The cell is typed `char`, so every store into it was a scalar value.
        StaticClass::Char => Value::Char(char::from_u32(bits as u32).unwrap()),
    }
}
