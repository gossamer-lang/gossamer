#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

use super::*;

// ---------------------------------------------------------------
// encoding::yaml - YAML 1.2 parsing + emission via `serde_norway`.
// Returns `Result<String, errors::Error>` for fallible operations.
// Mirrors the toml_enc.rs surface so the auto-derive synthesizer
// can reuse the same JSON-as-lingua-franca shape.
// ---------------------------------------------------------------

fn yaml_result_ok(s: &str) -> i128 {
    unsafe { gos_rt_result_new(0, alloc_cstring(s.as_bytes()) as i64) }
}

fn yaml_result_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    unsafe { gos_rt_result_new(1, err as i64) }
}

fn serde_norway_to_json(v: serde_norway::Value) -> serde_json::Value {
    match v {
        serde_norway::Value::Null => serde_json::Value::Null,
        serde_norway::Value::Bool(b) => serde_json::Value::Bool(b),
        serde_norway::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                serde_json::Value::Number(u.into())
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map_or(serde_json::Value::Null, serde_json::Value::Number)
            } else {
                serde_json::Value::Null
            }
        }
        serde_norway::Value::String(s) => serde_json::Value::String(s),
        serde_norway::Value::Sequence(items) => {
            serde_json::Value::Array(items.into_iter().map(serde_norway_to_json).collect())
        }
        serde_norway::Value::Mapping(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                let key = match &k {
                    serde_norway::Value::String(s) => s.clone(),
                    serde_norway::Value::Number(n) => n.to_string(),
                    serde_norway::Value::Bool(b) => b.to_string(),
                    _ => format!("{k:?}"),
                };
                obj.insert(key, serde_norway_to_json(v));
            }
            serde_json::Value::Object(obj)
        }
        serde_norway::Value::Tagged(t) => serde_norway_to_json(t.value),
    }
}

fn json_to_serde_norway(v: &serde_json::Value) -> serde_norway::Value {
    match v {
        serde_json::Value::Null => serde_norway::Value::Null,
        serde_json::Value::Bool(b) => serde_norway::Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_norway::Value::Number(i.into())
            } else if let Some(f) = n.as_f64() {
                serde_norway::Value::Number(serde_norway::Number::from(f))
            } else {
                serde_norway::Value::Null
            }
        }
        serde_json::Value::String(s) => serde_norway::Value::String(s.clone()),
        serde_json::Value::Array(items) => {
            serde_norway::Value::Sequence(items.iter().map(json_to_serde_norway).collect())
        }
        serde_json::Value::Object(map) => {
            let mut m = serde_norway::Mapping::new();
            for (k, v) in map {
                m.insert(
                    serde_norway::Value::String(k.clone()),
                    json_to_serde_norway(v),
                );
            }
            serde_norway::Value::Mapping(m)
        }
    }
}

/// `encoding::yaml::parse(text) -> Result<json::Value, Error>`.
/// YAML is parsed and re-projected onto the JSON value tree so the
/// dynamic document path reuses the fully-supported `json::Value`
/// runtime type (`json::get` / `as_str` / …) on every tier - the VM's
/// `yaml::parse` routes through the same yaml->json projection. Err
/// payload is a c-string, matching `gos_rt_json_parse`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_parse(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let text = if s.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        match serde_norway::from_str::<serde_norway::Value>(text) {
            Ok(yaml_val) => {
                let json_val = serde_norway_to_json(yaml_val);
                let ptr = crate::c_abi::json::GosJson::into_raw(json_val);
                unsafe { gos_rt_result_new(0, ptr as i64) }
            }
            Err(e) => {
                let cs = alloc_cstring(format!("yaml::parse: {e}").as_bytes());
                unsafe { gos_rt_result_new(1, cs as i64) }
            }
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_to_json(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let text = if s.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        let yaml_val: serde_norway::Value = match serde_norway::from_str(text) {
            Ok(v) => v,
            Err(e) => return yaml_result_err(&format!("yaml::to_json: {e}")),
        };
        let json_val = serde_norway_to_json(yaml_val);
        match serde_json::to_string(&json_val) {
            Ok(out) => yaml_result_ok(&out),
            Err(e) => yaml_result_err(&format!("yaml::to_json: {e}")),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_from_json(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let text = if s.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        let json_val: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => return yaml_result_err(&format!("yaml::from_json: {e}")),
        };
        let yaml_val = json_to_serde_norway(&json_val);
        match serde_norway::to_string(&yaml_val) {
            Ok(out) => yaml_result_ok(&out),
            Err(e) => yaml_result_err(&format!("yaml::from_json: {e}")),
        }
    })
}

/// `encoding::yaml::encode(value) -> Result<String, Error>`. Projects a
/// `json::Value` tree onto YAML and serialises it. Mirrors the interp's
/// `yaml::encode`, which converts through the JSON lingua franca then
/// emits via `serde_norway`. Err payload is an `errors::Error`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_encode(j: *const crate::c_abi::json::GosJson) -> i128 {
    ffi_entry!(0i128, {
        let yaml_val = match unsafe { crate::c_abi::json::json_value_ref(j) } {
            Some(jv) => json_to_serde_norway(jv),
            None => serde_norway::Value::Null,
        };
        match serde_norway::to_string(&yaml_val) {
            Ok(out) => yaml_result_ok(&out),
            Err(e) => yaml_result_err(&format!("yaml::encode: {e}")),
        }
    })
}

/// `encoding::yaml::parse_all(text) -> Result<Vec<json::Value>, Error>`.
/// Parses every document in a multi-document YAML stream, projecting
/// each onto the `json::Value` runtime type. The Ok payload is a
/// `*mut GosVec` of `*mut GosJson` handles (8-byte slots).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_parse_all(s: *const c_char) -> i128 {
    use serde::Deserialize;
    ffi_entry!(0i128, {
        let text = if s.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        // JSON-typed: each element is a handle holding a share of its
        // document, which `gos_rt_vec_free` gives back with the vec.
        let vec = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::JSON)
        };
        for doc in serde_norway::Deserializer::from_str(text) {
            match serde_norway::Value::deserialize(doc) {
                Ok(value) => {
                    let json_val = serde_norway_to_json(value);
                    let ptr = crate::c_abi::json::GosJson::into_raw(json_val);
                    unsafe { crate::c_abi::vec::gos_rt_vec_push_i64(vec, ptr as i64) };
                }
                Err(e) => return yaml_result_err(&format!("yaml::parse_all: {e}")),
            }
        }
        unsafe { gos_rt_result_new(0, vec as i64) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_is_valid(s: *const c_char) -> i64 {
    ffi_entry!(0, {
        let text = if s.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        i64::from(serde_norway::from_str::<serde_norway::Value>(text).is_ok())
    })
}
