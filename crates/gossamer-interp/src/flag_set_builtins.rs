//! VM hooks for the legacy builder-style `flag::Set` API
//! exercised by `examples/cli_args.gos` and `examples/grep.gos`.
//! Backed by a thread-local cell registry so `Set::parse` can
//! mutate values that are later read via `*cell` (deref).
//!
//! Kept as its own module so `builtins.rs` stays under the
//! 2000-line hard limit defined in `GUIDELINES.md`.

// Builtins return `RuntimeResult<Value>` to match the dispatcher's
// expected signature even when they never fail.
#![allow(clippy::unnecessary_wraps)]
use std::sync::Arc;

use crate::builtins::{
    CELL_REGISTRY, FlagDef, FlagKind, NEXT_SET_ID, SET_REGISTRY, SetState, as_str, make_cell,
    ok_variant, program_args, value_to_int,
};
use crate::value::{RuntimeError, RuntimeResult, SmolStr, Value};

pub(crate) fn builtin_flag_set_new(args: &[Value]) -> RuntimeResult<Value> {
    let _name = args.first().and_then(as_str).unwrap_or("");
    let id = NEXT_SET_ID.with(|cell| {
        let mut v = cell.borrow_mut();
        let id = *v;
        *v += 1;
        id
    });
    let set_name = args.first().and_then(as_str).unwrap_or("").to_string();
    SET_REGISTRY.with(|reg| {
        reg.borrow_mut().insert(
            id,
            SetState {
                name: set_name,
                flag_order: Vec::new(),
                last_flag: None,
                flags: std::collections::HashMap::new(),
            },
        );
    });
    Ok(Value::struct_("Set", vec![("__id", Value::Int(id as i64))]))
}

fn set_id_from_value(value: &Value) -> Option<u64> {
    match value {
        Value::Struct(inner) if inner.name == "Set" => inner
            .fields
            .iter()
            .find(|(ident, _)| (**ident) == "__id")
            .and_then(|(_, v)| match v {
                Value::Int(n) => Some(*n as u64),
                _ => None,
            }),
        _ => None,
    }
}

pub(crate) fn builtin_flag_set_string(args: &[Value]) -> RuntimeResult<Value> {
    let Some(set) = args.first() else {
        return Ok(Value::Unit);
    };
    let Some(id) = set_id_from_value(set) else {
        return Ok(Value::Unit);
    };
    let flag_name = args.get(1).and_then(as_str).unwrap_or("");
    let default = args.get(2).and_then(as_str).unwrap_or("").to_string();
    let help_text = args.get(3).and_then(as_str).unwrap_or("");
    SET_REGISTRY.with(|reg| {
        if let Some(state) = reg.borrow_mut().get_mut(&id) {
            state.last_flag = Some(flag_name.to_string());
            state.flag_order.retain(|n| n != flag_name);
            state.flag_order.push(flag_name.to_string());
            state.flags.insert(
                flag_name.to_string(),
                FlagDef {
                    short: None,
                    kind: FlagKind::String,
                    help: help_text.to_string(),
                    default: Value::String(SmolStr::from(default.clone())),
                },
            );
        }
    });
    Ok(make_cell(id, flag_name, Value::String(default.into())))
}

pub(crate) fn builtin_flag_set_int(args: &[Value]) -> RuntimeResult<Value> {
    let Some(set) = args.first() else {
        return Ok(Value::Unit);
    };
    let Some(id) = set_id_from_value(set) else {
        return Ok(Value::Unit);
    };
    let flag_name = args.get(1).and_then(as_str).unwrap_or("");
    let default = args.get(2).and_then(value_to_int).unwrap_or(0);
    let help_text = args.get(3).and_then(as_str).unwrap_or("");
    SET_REGISTRY.with(|reg| {
        if let Some(state) = reg.borrow_mut().get_mut(&id) {
            state.last_flag = Some(flag_name.to_string());
            state.flag_order.retain(|n| n != flag_name);
            state.flag_order.push(flag_name.to_string());
            state.flags.insert(
                flag_name.to_string(),
                FlagDef {
                    short: None,
                    kind: FlagKind::Int,
                    help: help_text.to_string(),
                    default: Value::Int(default),
                },
            );
        }
    });
    Ok(make_cell(id, flag_name, Value::Int(default)))
}

pub(crate) fn builtin_flag_set_uint(args: &[Value]) -> RuntimeResult<Value> {
    let Some(set) = args.first() else {
        return Ok(Value::Unit);
    };
    let Some(id) = set_id_from_value(set) else {
        return Ok(Value::Unit);
    };
    let flag_name = args.get(1).and_then(as_str).unwrap_or("");
    let default = args
        .get(2)
        .and_then(value_to_int)
        .and_then(|n| if n >= 0 { Some(n as u64) } else { None })
        .unwrap_or(0);
    let help_text = args.get(3).and_then(as_str).unwrap_or("");
    SET_REGISTRY.with(|reg| {
        if let Some(state) = reg.borrow_mut().get_mut(&id) {
            state.last_flag = Some(flag_name.to_string());
            state.flag_order.retain(|n| n != flag_name);
            state.flag_order.push(flag_name.to_string());
            state.flags.insert(
                flag_name.to_string(),
                FlagDef {
                    short: None,
                    kind: FlagKind::Uint,
                    help: help_text.to_string(),
                    default: Value::Int(default as i64),
                },
            );
        }
    });
    Ok(make_cell(id, flag_name, Value::Int(default as i64)))
}

pub(crate) fn builtin_flag_set_float(args: &[Value]) -> RuntimeResult<Value> {
    let Some(set) = args.first() else {
        return Ok(Value::Unit);
    };
    let Some(id) = set_id_from_value(set) else {
        return Ok(Value::Unit);
    };
    let flag_name = args.get(1).and_then(as_str).unwrap_or("");
    let default = match args.get(2) {
        Some(Value::Float(f)) => *f,
        Some(Value::Int(n)) => *n as f64,
        _ => 0.0,
    };
    let help_text = args.get(3).and_then(as_str).unwrap_or("");
    SET_REGISTRY.with(|reg| {
        if let Some(state) = reg.borrow_mut().get_mut(&id) {
            state.last_flag = Some(flag_name.to_string());
            state.flag_order.retain(|n| n != flag_name);
            state.flag_order.push(flag_name.to_string());
            state.flags.insert(
                flag_name.to_string(),
                FlagDef {
                    short: None,
                    kind: FlagKind::Float,
                    help: help_text.to_string(),
                    default: Value::Float(default),
                },
            );
        }
    });
    Ok(make_cell(id, flag_name, Value::Float(default)))
}

/// Duration cell - a `time::Duration` is a count of nanoseconds, so
/// the default is whatever `time::Duration::from_secs(n)` /
/// `from_millis(n)` produced.
pub(crate) fn builtin_flag_set_duration(args: &[Value]) -> RuntimeResult<Value> {
    let Some(set) = args.first() else {
        return Ok(Value::Unit);
    };
    let Some(id) = set_id_from_value(set) else {
        return Ok(Value::Unit);
    };
    let flag_name = args.get(1).and_then(as_str).unwrap_or("");
    let default = args.get(2).and_then(value_to_int).unwrap_or(0);
    let help_text = args.get(3).and_then(as_str).unwrap_or("");
    SET_REGISTRY.with(|reg| {
        if let Some(state) = reg.borrow_mut().get_mut(&id) {
            state.last_flag = Some(flag_name.to_string());
            state.flag_order.retain(|n| n != flag_name);
            state.flag_order.push(flag_name.to_string());
            state.flags.insert(
                flag_name.to_string(),
                FlagDef {
                    short: None,
                    kind: FlagKind::Duration,
                    help: help_text.to_string(),
                    default: Value::Int(default),
                },
            );
        }
    });
    Ok(make_cell(id, flag_name, Value::Int(default)))
}

pub(crate) fn builtin_flag_set_string_list(args: &[Value]) -> RuntimeResult<Value> {
    let Some(set) = args.first() else {
        return Ok(Value::Unit);
    };
    let Some(id) = set_id_from_value(set) else {
        return Ok(Value::Unit);
    };
    let flag_name = args.get(1).and_then(as_str).unwrap_or("");
    let help_text = args.get(2).and_then(as_str).unwrap_or("");
    SET_REGISTRY.with(|reg| {
        if let Some(state) = reg.borrow_mut().get_mut(&id) {
            state.last_flag = Some(flag_name.to_string());
            state.flag_order.retain(|n| n != flag_name);
            state.flag_order.push(flag_name.to_string());
            state.flags.insert(
                flag_name.to_string(),
                FlagDef {
                    short: None,
                    kind: FlagKind::StringList,
                    help: help_text.to_string(),
                    default: Value::Array(Arc::new(Vec::new())),
                },
            );
        }
    });
    Ok(make_cell(id, flag_name, Value::Array(Arc::new(Vec::new()))))
}

pub(crate) fn builtin_flag_set_usage(args: &[Value]) -> RuntimeResult<Value> {
    use std::fmt::Write as _;
    let Some(set) = args.first() else {
        return Ok(Value::String(SmolStr::from(String::new())));
    };
    let Some(id) = set_id_from_value(set) else {
        return Ok(Value::String(SmolStr::from(String::new())));
    };
    let state = SET_REGISTRY.with(|reg| reg.borrow().get(&id).cloned());
    let Some(state) = state else {
        return Ok(Value::String(SmolStr::from(String::new())));
    };
    let mut out = format!(
        "usage: {} [FLAGS] [POSITIONAL]\n\nflags:\n",
        if state.name.is_empty() {
            "program"
        } else {
            &state.name
        }
    );
    for name in &state.flag_order {
        let Some(def) = state.flags.get(name) else {
            continue;
        };
        let label = match def.short {
            Some(ch) => format!("  -{ch}, --{name}"),
            None => format!("      --{name}"),
        };
        let _ = writeln!(out, "{label:<30} {}", def.help);
    }
    Ok(Value::String(SmolStr::from(out)))
}

pub(crate) fn builtin_flag_set_bool(args: &[Value]) -> RuntimeResult<Value> {
    let Some(set) = args.first() else {
        return Ok(Value::Unit);
    };
    let Some(id) = set_id_from_value(set) else {
        return Ok(Value::Unit);
    };
    let flag_name = args.get(1).and_then(as_str).unwrap_or("");
    let default = match args.get(2) {
        Some(Value::Bool(b)) => *b,
        _ => false,
    };
    let help_text = args.get(3).and_then(as_str).unwrap_or("");
    SET_REGISTRY.with(|reg| {
        if let Some(state) = reg.borrow_mut().get_mut(&id) {
            state.last_flag = Some(flag_name.to_string());
            state.flag_order.retain(|n| n != flag_name);
            state.flag_order.push(flag_name.to_string());
            state.flags.insert(
                flag_name.to_string(),
                FlagDef {
                    short: None,
                    kind: FlagKind::Bool,
                    help: help_text.to_string(),
                    default: Value::Bool(default),
                },
            );
        }
    });
    Ok(make_cell(id, flag_name, Value::Bool(default)))
}

pub(crate) fn builtin_flag_set_short(args: &[Value]) -> RuntimeResult<Value> {
    let Some(set) = args.first() else {
        return Ok(Value::Unit);
    };
    let Some(id) = set_id_from_value(set) else {
        return Ok(Value::Unit);
    };
    let letter = match args.get(1) {
        Some(Value::Char(c)) => *c,
        _ => return Ok(Value::Unit),
    };
    SET_REGISTRY.with(|reg| {
        let mut reg = reg.borrow_mut();
        let Some(state) = reg.get_mut(&id) else {
            return;
        };
        let Some(last) = state.last_flag.clone() else {
            return;
        };
        if let Some(def) = state.flags.get_mut(&last) {
            def.short = Some(letter);
        }
    });
    Ok(Value::Unit)
}

pub(crate) fn builtin_flag_set_parse(args: &[Value]) -> RuntimeResult<Value> {
    let Some(set) = args.first() else {
        return Ok(ok_variant(Value::empty_array()));
    };
    let Some(id) = set_id_from_value(set) else {
        return Ok(ok_variant(Value::empty_array()));
    };
    let program_args: Vec<String> = match args.get(1) {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.as_str().to_string()),
                _ => None,
            })
            .collect(),
        _ => program_args(),
    };
    let state = SET_REGISTRY.with(|reg| reg.borrow().get(&id).cloned());
    let Some(state) = state else {
        return Ok(ok_variant(Value::empty_array()));
    };

    // Auto-generated `--help` / `-h`. Prints a usage line, the
    // registered flag table, and exits the process with status 0 so
    // callers of flags.parse don't need to thread a check through
    // every program.
    if program_args.iter().any(|a| a == "--help" || a == "-h") {
        print_flag_help(&state);
        if gossamer_runtime::platform::CAN_END_PROCESS {
            std::process::exit(0);
        }
        return Err(RuntimeError::Exit(0));
    }
    let mut positional: Vec<Value> = Vec::new();
    let mut idx = 0usize;
    while idx < program_args.len() {
        let arg = &program_args[idx];
        if arg == "--" {
            for rest in &program_args[idx + 1..] {
                positional.push(Value::String(SmolStr::from(rest.clone())));
            }
            break;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            idx += parse_long_flag(id, rest, &program_args, idx, &state);
            continue;
        }
        if let Some(rest) = arg.strip_prefix('-') {
            if rest.is_empty() {
                positional.push(Value::String(SmolStr::from(arg.clone())));
                idx += 1;
                continue;
            }
            idx += parse_short_flag(id, rest, &program_args, idx, &state);
            continue;
        }
        positional.push(Value::String(SmolStr::from(arg.clone())));
        idx += 1;
    }

    Ok(ok_variant(Value::Array(Arc::new(positional))))
}

fn print_flag_help(state: &SetState) {
    let program = if state.name.is_empty() {
        "program"
    } else {
        &state.name
    };
    println!("Usage: {program} [OPTIONS]");
    if state.flag_order.is_empty() {
        return;
    }
    println!();
    println!("Options:");
    let mut col = Vec::new();
    for name in &state.flag_order {
        let Some(def) = state.flags.get(name) else {
            continue;
        };
        let short = def
            .short
            .map_or_else(|| "    ".to_string(), |c| format!("-{c}, "));
        let value_hint = match def.kind {
            FlagKind::Bool => String::new(),
            FlagKind::String => " <STRING>".to_string(),
            FlagKind::Int => " <INT>".to_string(),
            FlagKind::Uint => " <UINT>".to_string(),
            FlagKind::Float => " <FLOAT>".to_string(),
            FlagKind::Duration => " <DURATION>".to_string(),
            FlagKind::StringList => " <STRING>".to_string(),
        };
        let flag_col = format!("  {short}--{name}{value_hint}");
        col.push((flag_col, def));
    }
    let max_width = col
        .iter()
        .map(|(c, _)| c.chars().count())
        .max()
        .unwrap_or(0);
    for (flag_col, def) in col {
        let pad = max_width.saturating_sub(flag_col.chars().count());
        let default_text = match &def.default {
            Value::String(s) if !s.is_empty() => format!(" [default: {s}]"),
            Value::Int(n) => format!(" [default: {n}]"),
            Value::Bool(true) => " [default: true]".to_string(),
            Value::Bool(false) => " [default: false]".to_string(),
            _ => String::new(),
        };
        println!("{flag_col}{}  {}{default_text}", " ".repeat(pad), def.help);
    }
    println!(
        "      --help, -h{}  print this message and exit",
        " ".repeat(max_width.saturating_sub(15))
    );
}

fn parse_long_flag(
    set_id: u64,
    rest: &str,
    program_args: &[String],
    idx: usize,
    state: &SetState,
) -> usize {
    let (name, explicit) = match rest.split_once('=') {
        Some((n, v)) => (n.to_string(), Some(v.to_string())),
        None => (rest.to_string(), None),
    };
    let Some(def) = state.flags.get(&name) else {
        return 1;
    };
    let (parsed, consumed) = if let Some(v) = explicit {
        (set_parse_value(def, &v), 0)
    } else if matches!(def.kind, FlagKind::Bool) {
        (Value::Bool(true), 0)
    } else {
        let Some(next) = program_args.get(idx + 1) else {
            return 1;
        };
        (set_parse_value(def, next), 1)
    };
    CELL_REGISTRY.with(|reg| {
        if let Some(cell) = reg.borrow().get(&(set_id, name.clone())) {
            store_or_append_cell(cell, &def.kind, parsed);
        }
    });
    1 + consumed
}

fn parse_short_flag(
    set_id: u64,
    rest: &str,
    program_args: &[String],
    idx: usize,
    state: &SetState,
) -> usize {
    let mut chars = rest.chars();
    let first = chars.next().unwrap();
    let remainder = chars.as_str();
    let Some((flag_name, def)) = state.flags.iter().find(|(_, d)| d.short == Some(first)) else {
        return 1;
    };
    let flag_name = flag_name.clone();
    let explicit = if remainder.is_empty() {
        None
    } else {
        Some(remainder.to_string())
    };
    let (parsed, consumed) = if let Some(v) = explicit {
        (set_parse_value(def, &v), 0)
    } else if matches!(def.kind, FlagKind::Bool) {
        (Value::Bool(true), 0)
    } else {
        let Some(next) = program_args.get(idx + 1) else {
            return 1;
        };
        (set_parse_value(def, next), 1)
    };
    CELL_REGISTRY.with(|reg| {
        if let Some(cell) = reg.borrow().get(&(set_id, flag_name.clone())) {
            store_or_append_cell(cell, &def.kind, parsed);
        }
    });
    1 + consumed
}

fn store_or_append_cell(
    cell: &std::sync::Arc<parking_lot::Mutex<Value>>,
    kind: &FlagKind,
    parsed: Value,
) {
    let mut slot = cell.lock();
    match kind {
        FlagKind::StringList => {
            let mut items: Vec<Value> = match &*slot {
                Value::Array(arr) => arr.iter().cloned().collect(),
                _ => Vec::new(),
            };
            items.push(parsed);
            *slot = Value::Array(Arc::new(items));
        }
        _ => {
            *slot = parsed;
        }
    }
}

fn set_parse_value(def: &FlagDef, raw: &str) -> Value {
    match def.kind {
        FlagKind::String | FlagKind::StringList => Value::String(SmolStr::from(raw.to_string())),
        FlagKind::Int => Value::Int(raw.parse::<i64>().unwrap_or(0)),
        FlagKind::Uint => Value::Int(raw.parse::<u64>().unwrap_or(0) as i64),
        FlagKind::Float => Value::Float(raw.parse::<f64>().unwrap_or(0.0)),
        FlagKind::Bool => {
            let b = matches!(
                raw,
                "true" | "1" | "yes" | "on" | "false" | "0" | "no" | "off"
            );
            // Reject anything that wasn't on the recognized list.
            if matches!(raw, "true" | "1" | "yes" | "on") {
                Value::Bool(true)
            } else if matches!(raw, "false" | "0" | "no" | "off") {
                Value::Bool(false)
            } else if b {
                Value::Bool(true)
            } else {
                Value::Bool(false)
            }
        }
        FlagKind::Duration => {
            Value::Int(gossamer_runtime::c_abi::flag::parse_duration_text(raw).unwrap_or(0))
        }
    }
}
