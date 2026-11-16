//! JSON text re-rendered as YAML or TOML.
//!
//! A typed `to_yaml` / `to_toml` encodes through JSON text whose members are
//! already in the order the walk chose (declaration order, or by key under
//! `sort: true`), so it keeps the text's order. A conversion of a document
//! (`yaml::from_json_text`) orders every object's members by key, as a
//! `json::Value` holds them.

/// How a conversion orders an object's members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Members {
    /// In the order the JSON text has them.
    AsWritten,
    /// By key, as `serde_json::Value` and `toml::Table` hold them.
    Sorted,
}

/// Re-renders a JSON document as YAML.
pub fn to_yaml(json_text: &str, members: Members) -> Result<String, String> {
    let value = parse(json_text, members)?;
    serde_norway::to_string(&value).map_err(|e| e.to_string())
}

/// Re-renders a JSON document as TOML.
///
/// The top level must be an object. TOML has no null and no integer past
/// `i64::MAX`, so either is an error rather than a dropped or rounded value.
pub fn to_toml(json_text: &str, members: Members) -> Result<String, String> {
    let value = parse(json_text, members)?;
    if !value.is_mapping() {
        return Err(format!(
            "TOML needs a table at the top level, found {}",
            kind(&value)
        ));
    }
    check_toml(&value)?;
    toml::to_string_pretty(&value).map_err(|e| e.to_string())
}

/// A TOML document as JSON text, each table's members by key.
pub fn toml_to_json(toml_text: &str) -> Result<String, String> {
    let value: toml::Value = toml::from_str(toml_text).map_err(|e| e.to_string())?;
    serde_json::to_string(&toml_value_to_json(&value)).map_err(|e| e.to_string())
}

/// A TOML value as the JSON value it reads as: a datetime as its text.
pub fn toml_value_to_json(v: &toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(toml_value_to_json).collect())
        }
        toml::Value::Table(t) => {
            let mut map = serde_json::Map::new();
            for (k, v) in t {
                map.insert(k.clone(), toml_value_to_json(v));
            }
            serde_json::Value::Object(map)
        }
    }
}

// `serde_norway::Mapping` keeps insertion order, where `serde_json::Map`
// without `preserve_order` re-sorts members by key.
fn parse(json_text: &str, members: Members) -> Result<serde_norway::Value, String> {
    let mut value = serde_json::from_str(json_text).map_err(|e| e.to_string())?;
    if members == Members::Sorted {
        sort_members(&mut value);
    }
    Ok(value)
}

/// Orders every mapping's members by key, at every depth.
fn sort_members(value: &mut serde_norway::Value) {
    use serde_norway::Value;
    match value {
        Value::Sequence(items) => items.iter_mut().for_each(sort_members),
        Value::Mapping(map) => {
            let mut members: Vec<(Value, Value)> = std::mem::take(map).into_iter().collect();
            members.sort_by(|(a, _), (b, _)| a.as_str().cmp(&b.as_str()));
            for (key, mut member) in members {
                sort_members(&mut member);
                map.insert(key, member);
            }
        }
        Value::Tagged(tagged) => sort_members(&mut tagged.value),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn check_toml(value: &serde_norway::Value) -> Result<(), String> {
    use serde_norway::Value;
    match value {
        Value::Null => Err("TOML has no null".to_string()),
        Value::Number(n) if n.is_u64() && n.as_i64().is_none() => {
            Err(format!("integer {n} is outside TOML's range (i64)"))
        }
        Value::Sequence(items) => items.iter().try_for_each(check_toml),
        Value::Mapping(map) => map.values().try_for_each(check_toml),
        Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Tagged(_) => Ok(()),
    }
}

fn kind(value: &serde_norway::Value) -> &'static str {
    use serde_norway::Value;
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Sequence(_) => "an array",
        Value::Mapping(_) | Value::Tagged(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_keeps_member_order() {
        let out = to_yaml(r#"{"name":"a","n":2,"f":1.0}"#, Members::AsWritten).unwrap();
        assert_eq!(out, "name: a\nn: 2\nf: 1.0\n");
    }

    #[test]
    fn yaml_keeps_integers_past_i64_exact() {
        let out = to_yaml(r#"{"big":18446744073709551615}"#, Members::AsWritten).unwrap();
        assert_eq!(out, "big: 18446744073709551615\n");
    }

    #[test]
    fn toml_keeps_member_order_and_puts_tables_last() {
        let out = to_toml(r#"{"q":{"a":1},"name":"x","n":2}"#, Members::AsWritten).unwrap();
        assert_eq!(out, "name = \"x\"\nn = 2\n\n[q]\na = 1\n");
    }

    #[test]
    fn toml_refuses_null_and_out_of_range_integers() {
        assert_eq!(
            to_toml(r#"{"a":null}"#, Members::AsWritten).unwrap_err(),
            "TOML has no null"
        );
        assert!(
            to_toml(r#"{"a":9223372036854775808}"#, Members::AsWritten)
                .unwrap_err()
                .contains("outside")
        );
        assert!(
            to_toml("[1]", Members::AsWritten)
                .unwrap_err()
                .contains("found an array")
        );
    }
}
