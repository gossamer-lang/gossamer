//! Helpers the `#[derive(GosStruct)]` proc-macro emits calls to.

use crate::value::Value;

/// A struct value named `name` with `fields`.
#[must_use]
pub fn build_struct(name: &str, fields: Vec<(String, Value)>) -> Value {
    Value::struct_(name, fields)
}

/// The value of the field `name`, or the unit value when there is none.
#[must_use]
pub fn struct_field<'a>(fields: &'a [(String, Value)], name: &str) -> &'a Value {
    static UNIT: Value = Value::Unit;
    fields
        .iter()
        .find_map(|(k, v)| (k == name).then_some(v))
        .unwrap_or(&UNIT)
}
