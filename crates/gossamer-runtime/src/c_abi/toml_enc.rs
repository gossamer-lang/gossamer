#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

use super::*;

// ---------------------------------------------------------------
// encoding::toml - TOML 1.0 parsing + emission. Returns
// `Result<String, errors::Error>` for fallible operations.
// ---------------------------------------------------------------

fn toml_value_to_json_value(v: &toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(toml_value_to_json_value).collect())
        }
        toml::Value::Table(t) => {
            let mut map = serde_json::Map::new();
            for (k, v) in t {
                map.insert(k.clone(), toml_value_to_json_value(v));
            }
            serde_json::Value::Object(map)
        }
    }
}

fn json_value_to_toml_value(v: &serde_json::Value) -> Result<toml::Value, String> {
    match v {
        serde_json::Value::Null => Err("TOML has no null".to_string()),
        serde_json::Value::Bool(b) => Ok(toml::Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(toml::Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(toml::Value::Float(f))
            } else {
                Err(format!("unrepresentable number: {n}"))
            }
        }
        serde_json::Value::String(s) => Ok(toml::Value::String(s.clone())),
        serde_json::Value::Array(items) => Ok(toml::Value::Array(
            items
                .iter()
                .map(json_value_to_toml_value)
                .collect::<Result<_, _>>()?,
        )),
        serde_json::Value::Object(map) => {
            let mut t = toml::value::Table::new();
            for (k, v) in map {
                t.insert(k.clone(), json_value_to_toml_value(v)?);
            }
            Ok(toml::Value::Table(t))
        }
    }
}

fn toml_result_ok(s: &str) -> i128 {
    gos_rt_result_new(0, alloc_cstring(s.as_bytes()) as i64)
}

fn toml_result_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    gos_rt_result_new(1, err as i64)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_toml_to_json(s: *const c_char) -> i128 {
    ffi_entry!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        let value: toml::Value = match toml::from_str(text) {
            Ok(v) => v,
            Err(e) => return toml_result_err(&format!("toml::to_json: {e}")),
        };
        let json = toml_value_to_json_value(&value);
        match serde_json::to_string(&json) {
            Ok(s) => toml_result_ok(&s),
            Err(e) => toml_result_err(&format!("toml::to_json: {e}")),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_toml_from_json(s: *const c_char) -> i128 {
    ffi_entry!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        let v: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => return toml_result_err(&format!("toml::from_json: {e}")),
        };
        let tv = match json_value_to_toml_value(&v) {
            Ok(v) => v,
            Err(e) => return toml_result_err(&format!("toml::from_json: {e}")),
        };
        match toml::to_string_pretty(&tv) {
            Ok(s) => toml_result_ok(&s),
            Err(e) => toml_result_err(&format!("toml::from_json: {e}")),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_toml_is_valid(s: *const c_char) -> i64 {
    ffi_entry!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        i64::from(toml::from_str::<toml::Value>(text).is_ok())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_toml_pretty(s: *const c_char) -> i128 {
    ffi_entry!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        let value: toml::Value = match toml::from_str(text) {
            Ok(v) => v,
            Err(e) => return toml_result_err(&format!("toml::pretty: {e}")),
        };
        match toml::to_string_pretty(&value) {
            Ok(s) => toml_result_ok(&s),
            Err(e) => toml_result_err(&format!("toml::pretty: {e}")),
        }
    })
}
