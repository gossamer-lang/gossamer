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

fn toml_result_ok(s: &str) -> i128 {
    gos_rt_result_new(0, alloc_cstring(s.as_bytes()) as i64)
}

fn toml_result_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    gos_rt_result_new(1, err as i64)
}

/// `toml::to_json_text(text) -> Result<String, Error>`: the document as JSON
/// text, each table's members by key.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_toml_to_json_text(s: *const c_char) -> i128 {
    ffi_entry!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        let value: toml::Value = match toml::from_str(text) {
            Ok(v) => v,
            Err(e) => return toml_result_err(&format!("toml::to_json_text: {e}")),
        };
        let json = crate::codec::json_convert::toml_value_to_json(&value);
        match serde_json::to_string(&json) {
            Ok(s) => toml_result_ok(&s),
            Err(e) => toml_result_err(&format!("toml::to_json_text: {e}")),
        }
    })
}

/// `toml::from_json_text(text) -> Result<String, Error>`: JSON text as TOML,
/// each object's members by key, as a `json::Value` holds them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_toml_from_json_text(s: *const c_char) -> i128 {
    ffi_entry!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        match crate::codec::json_convert::to_toml(text, crate::codec::json_convert::Members::Sorted)
        {
            Ok(s) => toml_result_ok(&s),
            Err(e) => toml_result_err(&format!("toml::from_json_text: {e}")),
        }
    })
}

/// `toml::parse(text) -> Result<json::Value, Error>`: the document as a
/// `json::Value`, as `yaml::parse` answers one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_toml_parse(s: *const c_char) -> i128 {
    ffi_entry!({
        let text = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        match toml::from_str::<toml::Value>(text) {
            Ok(value) => {
                let handle = crate::c_abi::json::GosJson::into_raw(
                    crate::codec::json_convert::toml_value_to_json(&value),
                );
                gos_rt_result_new(0, handle as i64)
            }
            Err(e) => toml_result_err(&format!("toml::parse: {e}")),
        }
    })
}

/// `toml::encode(value) -> Result<String, Error>`: a `json::Value` object as
/// a TOML document, its members by key.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_toml_encode(j: *const crate::c_abi::json::GosJson) -> i128 {
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call or null.
        let text = unsafe { crate::c_abi::json::json_value_text(j) };
        match crate::codec::json_convert::to_toml(
            &text,
            crate::codec::json_convert::Members::Sorted,
        ) {
            Ok(s) => toml_result_ok(&s),
            Err(e) => toml_result_err(&format!("toml::encode: {e}")),
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
