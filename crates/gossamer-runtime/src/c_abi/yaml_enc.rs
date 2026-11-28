#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
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
    gos_rt_result_new(0, alloc_cstring(s.as_bytes()) as i64)
}

fn yaml_result_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    gos_rt_result_new(1, err as i64)
}

fn json_to_serde_norway(v: &serde_json::Value) -> serde_norway::Value {
    match v {
        serde_json::Value::Null => serde_norway::Value::Null,
        serde_json::Value::Bool(b) => serde_norway::Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_norway::Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                serde_norway::Value::Number(u.into())
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
/// `yaml::parse` routes through the same yaml->json projection. The Err
/// payload is an `errors::Error`, as `gos_rt_json_parse`'s is.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_parse(s: *const c_char) -> i128 {
    // SAFETY: the caller's contract is this one's.
    unsafe { gos_rt_yaml_parse_with(s, 0) }
}

/// `yaml::parse(text, duplicate_keys: ..)`: [`gos_rt_yaml_parse`], reading
/// a key a mapping repeats as the `json::DuplicateKeys` policy index says.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_parse_with(s: *const c_char, duplicates: i64) -> i128 {
    ffi_entry!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        match crate::yaml_node::decode_json_with(
            text,
            crate::yaml_node::max_depth(),
            crate::yaml_node::max_size(),
            crate::yaml_node::DuplicateKeys::from_index(duplicates),
        ) {
            Ok(json_val) => {
                let ptr = crate::c_abi::json::GosJson::into_raw(json_val);
                gos_rt_result_new(0, ptr as i64)
            }
            Err(e) => yaml_result_err(&format!("yaml::parse: {e}")),
        }
    })
}

/// `yaml::to_json_text(text) -> Result<String, Error>`: the document as JSON
/// text, each object's members by key.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_to_json_text(s: *const c_char) -> i128 {
    ffi_entry!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        let json_val = match crate::yaml_node::decode_json(
            text,
            crate::yaml_node::max_depth(),
            crate::yaml_node::max_size(),
        ) {
            Ok(v) => v,
            Err(e) => return yaml_result_err(&format!("yaml::to_json_text: {e}")),
        };
        match serde_json::to_string(&json_val) {
            Ok(out) => yaml_result_ok(&out),
            Err(e) => yaml_result_err(&format!("yaml::to_json_text: {e}")),
        }
    })
}

/// `yaml::from_json_text(text) -> Result<String, Error>`: JSON text as YAML,
/// each object's members by key, as a `json::Value` holds them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_from_json_text(s: *const c_char) -> i128 {
    ffi_entry!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        match crate::codec::json_convert::to_yaml(text, crate::codec::json_convert::Members::Sorted)
        {
            Ok(out) => yaml_result_ok(&out),
            Err(e) => yaml_result_err(&format!("yaml::from_json_text: {e}")),
        }
    })
}

/// `encoding::yaml::encode(value) -> Result<String, Error>`. Projects a
/// `json::Value` tree onto YAML and serialises it. Mirrors the interp's
/// `yaml::encode`, which converts through the JSON lingua franca then
/// emits via `serde_norway`. Err payload is an `errors::Error`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_encode(j: *const crate::c_abi::json::GosJson) -> i128 {
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_value_ref` accepts.
        let yaml_val = match unsafe { crate::c_abi::json::json_value_ref(j) } {
            Some(jv) => json_to_serde_norway(&jv),
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
    ffi_entry!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        let docs = match crate::yaml_node::decode_all_json(
            text,
            crate::yaml_node::max_depth(),
            crate::yaml_node::max_size(),
        ) {
            Ok(docs) => docs,
            Err(e) => return yaml_result_err(&format!("yaml::parse_all: {e}")),
        };
        // JSON-typed: each element is a handle holding a share of its
        // document, which `gos_rt_vec_free` gives back with the vec.
        let vec =
            { crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::JSON) };
        for doc in docs {
            let ptr = crate::c_abi::json::GosJson::into_raw(doc);
            // SAFETY: `vec` is the fresh vec made above, or null, which `gos_rt_vec_push_i64`
            // accepts.
            unsafe { crate::c_abi::vec::gos_rt_vec_push_i64(vec, ptr as i64) };
        }
        gos_rt_result_new(0, vec as i64)
    })
}

/// `yaml::encode_all(values) -> Result<String, Error>`: a multi-document
/// stream, one document per `json::Value`, as `parse_all` reads it back.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_encode_all(values: *const crate::c_abi::vec::GosVec) -> i128 {
    ffi_entry!({
        // SAFETY: `values` is this shim's `Vec<json::Value>` argument, live for the call or null.
        let count = unsafe { crate::c_abi::vec::gos_rt_vec_len(values) };
        let mut out = String::new();
        for index in 0..count {
            // SAFETY: `values` is non-null when its length is positive, and
            // `index` is below that length; each slot holds a JSON handle.
            let handle = unsafe { crate::c_abi::vec::vec_elem_load_i64(&*values, index) };
            // SAFETY: the slot's handle is live while the vec is.
            let yaml_val = match unsafe {
                crate::c_abi::json::json_value_ref(handle as *const crate::c_abi::json::GosJson)
            } {
                Some(jv) => json_to_serde_norway(&jv),
                None => serde_norway::Value::Null,
            };
            if index > 0 {
                out.push_str("---\n");
            }
            match serde_norway::to_string(&yaml_val) {
                Ok(text) => out.push_str(&text),
                Err(e) => return yaml_result_err(&format!("yaml::encode_all: {e}")),
            }
        }
        yaml_result_ok(&out)
    })
}

/// `yaml::set_max_size(bytes)`: the size cap later YAML parses apply.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_yaml_set_max_size(bytes: i64) {
    ffi_entry!({
        crate::yaml_node::set_max_size(gossamer_core::json::cap_from_program(bytes));
    });
}

/// `yaml::set_max_depth(levels)`: the nesting cap later YAML parses apply.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_yaml_set_max_depth(levels: i64) {
    ffi_entry!({
        crate::yaml_node::set_max_depth(gossamer_core::json::cap_from_program(levels));
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_yaml_is_valid(s: *const c_char) -> i64 {
    ffi_entry!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        i64::from(crate::yaml_node::parse(text).is_ok())
    })
}
