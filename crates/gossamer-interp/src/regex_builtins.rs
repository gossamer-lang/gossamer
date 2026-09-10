//! VM hooks for `std::regex`. An opaque Pattern handle
//! is stashed in a process-global registry; the `Value::Struct`
//! exposed to Gossamer carries the handle id alongside the
//! original source so diagnostics can render the pattern.

use std::cell::RefCell;
use std::sync::Arc;

use gossamer_std::regex as regex_std;

use crate::stdlib_builtins::set::GlobalReg;
use crate::value::{RuntimeError, RuntimeResult, SmolStr, Value};

/// Flat list of `(short-name, fn pointer)` entries passed to the
/// shared `install_module` helper in `builtins.rs`. Adding a new
/// `regex::*` builtin only requires extending this table and
/// defining the function below.
type Entry = fn(&[Value]) -> RuntimeResult<Value>;

pub(crate) const ENTRIES: &[(&str, Entry)] = &[
    ("compile", builtin_regex_compile),
    ("is_match", builtin_regex_is_match),
    ("find", builtin_regex_find),
    ("find_all", builtin_regex_find_all),
    ("count", builtin_regex_count),
    ("captures", builtin_regex_captures),
    ("captures_all", builtin_regex_captures_all),
    ("replace", builtin_regex_replace),
    ("replace_all", builtin_regex_replace_all),
    ("split", builtin_regex_split),
];

// ------------------------------------------------------------------
// regex builtins - opaque handle backed by REGEX_REGISTRY.

// Process-global (not `thread_local!`): goroutines run on an OS
// worker-thread pool, so a compiled-pattern handle minted on one thread
// must resolve on another after the goroutine migrates between workers
// (a regex compiled before a `go`/channel hand-off has to match inside
// the spawned goroutine). `regex_std::Pattern` is `Send` (wraps a
// `regex::Regex`), so it is safe to share behind the global mutex.
// Mirrors the `sync::*` registries.
static NEXT_REGEX_ID: GlobalReg<u64> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(1)));
static REGEX_REGISTRY: GlobalReg<std::collections::HashMap<u64, regex_std::Pattern>> =
    GlobalReg::new(|| {
        parking_lot::ReentrantMutex::new(RefCell::new(std::collections::HashMap::new()))
    });

fn regex_handle(id: u64, source: &str) -> Value {
    Value::struct_(
        "regex::Pattern",
        vec![
            ("__regex_id", Value::Int(id as i64)),
            ("__source", Value::String(SmolStr::from(source.to_string()))),
        ],
    )
}

fn regex_id_from(value: &Value) -> RuntimeResult<u64> {
    let Value::Struct(inner) = value else {
        return Err(RuntimeError::Type(
            "regex: expected Pattern handle".to_string(),
        ));
    };
    if inner.name != "regex::Pattern" {
        return Err(RuntimeError::Type(
            "regex: expected Pattern handle".to_string(),
        ));
    }
    for (ident, v) in &inner.fields {
        if (*ident) == "__regex_id" {
            if let Value::Int(id) = v {
                return Ok(*id as u64);
            }
        }
    }
    Err(RuntimeError::Type(
        "regex: Pattern handle missing id".to_string(),
    ))
}

fn with_regex<T>(value: &Value, f: impl FnOnce(&regex_std::Pattern) -> T) -> RuntimeResult<T> {
    let id = regex_id_from(value)?;
    REGEX_REGISTRY.with(|reg| {
        reg.borrow()
            .get(&id)
            .map(f)
            .ok_or_else(|| RuntimeError::Type("regex: Pattern handle is stale".to_string()))
    })
}

fn arg_string<'a>(args: &'a [Value], idx: usize, context: &'static str) -> RuntimeResult<&'a str> {
    match args.get(idx) {
        Some(Value::String(s)) => Ok(s.as_str()),
        _ => Err(RuntimeError::Type(format!(
            "{context}: expected string argument"
        ))),
    }
}

fn match_triple(start: usize, end: usize, text: String) -> Value {
    Value::Tuple(Arc::from(vec![
        Value::Int(start as i64),
        Value::Int(end as i64),
        Value::String(text.into()),
    ]))
}

fn captures_to_array(caps: Vec<Option<String>>) -> Value {
    Value::Array(Arc::new(
        caps.into_iter()
            .map(|opt| match opt {
                Some(s) => Value::variant("Some", vec![Value::String(s.into())]),
                None => Value::variant("None", Vec::new()),
            })
            .collect(),
    ))
}

fn builtin_regex_compile(args: &[Value]) -> RuntimeResult<Value> {
    let pattern = arg_string(args, 0, "regex::compile")?;
    match regex_std::compile(pattern) {
        Ok(p) => {
            let id = NEXT_REGEX_ID.with(|cell| {
                let mut v = cell.borrow_mut();
                let id = *v;
                *v += 1;
                id
            });
            REGEX_REGISTRY.with(|reg| {
                reg.borrow_mut().insert(id, p);
            });
            Ok(Value::variant("Ok", vec![regex_handle(id, pattern)]))
        }
        Err(err) => Ok(Value::variant(
            "Err",
            vec![Value::String(SmolStr::from(err.to_string()))],
        )),
    }
}

fn builtin_regex_is_match(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .ok_or_else(|| RuntimeError::Type("regex::is_match: missing Pattern".to_string()))?;
    let text = arg_string(args, 1, "regex::is_match")?;
    let matched = with_regex(handle, |p| regex_std::is_match(p, text))?;
    Ok(Value::Bool(matched))
}

fn builtin_regex_find(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .ok_or_else(|| RuntimeError::Type("regex::find: missing Pattern".to_string()))?;
    let text = arg_string(args, 1, "regex::find")?;
    let hit = with_regex(handle, |p| regex_std::find(p, text))?;
    Ok(match hit {
        Some((s, e, t)) => Value::variant("Some", vec![match_triple(s, e, t)]),
        None => Value::variant("None", vec![]),
    })
}

fn builtin_regex_find_all(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .ok_or_else(|| RuntimeError::Type("regex::find_all: missing Pattern".to_string()))?;
    let text = arg_string(args, 1, "regex::find_all")?;
    let hits = with_regex(handle, |p| regex_std::find_all(p, text))?;
    Ok(Value::Array(Arc::new(
        hits.into_iter()
            .map(|(s, e, t)| match_triple(s, e, t))
            .collect(),
    )))
}

fn builtin_regex_count(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .ok_or_else(|| RuntimeError::Type("regex::count: missing Pattern".to_string()))?;
    let text = arg_string(args, 1, "regex::count")?;
    let hits = with_regex(handle, |p| regex_std::count(p, text))?;
    Ok(Value::Int(i64::try_from(hits).unwrap_or(i64::MAX)))
}

fn builtin_regex_captures(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .ok_or_else(|| RuntimeError::Type("regex::captures: missing Pattern".to_string()))?;
    let text = arg_string(args, 1, "regex::captures")?;
    let caps = with_regex(handle, |p| regex_std::captures(p, text))?;
    Ok(match caps {
        Some(groups) => Value::variant("Some", vec![captures_to_array(groups)]),
        None => Value::variant("None", vec![]),
    })
}

fn builtin_regex_captures_all(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .ok_or_else(|| RuntimeError::Type("regex::captures_all: missing Pattern".to_string()))?;
    let text = arg_string(args, 1, "regex::captures_all")?;
    let rows = with_regex(handle, |p| regex_std::captures_all(p, text))?;
    Ok(Value::Array(Arc::new(
        rows.into_iter().map(captures_to_array).collect(),
    )))
}

fn builtin_regex_replace(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .ok_or_else(|| RuntimeError::Type("regex::replace: missing Pattern".to_string()))?;
    let text = arg_string(args, 1, "regex::replace")?;
    let repl = arg_string(args, 2, "regex::replace")?;
    let out = with_regex(handle, |p| regex_std::replace(p, text, repl))?;
    Ok(Value::String(out.into()))
}

fn builtin_regex_replace_all(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .ok_or_else(|| RuntimeError::Type("regex::replace_all: missing Pattern".to_string()))?;
    let text = arg_string(args, 1, "regex::replace_all")?;
    let repl = arg_string(args, 2, "regex::replace_all")?;
    let out = with_regex(handle, |p| regex_std::replace_all(p, text, repl))?;
    Ok(Value::String(out.into()))
}

fn builtin_regex_split(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .ok_or_else(|| RuntimeError::Type("regex::split: missing Pattern".to_string()))?;
    let text = arg_string(args, 1, "regex::split")?;
    let parts = with_regex(handle, |p| regex_std::split(p, text))?;
    Ok(Value::Array(Arc::new(
        parts.into_iter().map(|s| Value::String(s.into())).collect(),
    )))
}
