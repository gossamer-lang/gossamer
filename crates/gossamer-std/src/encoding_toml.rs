// `std::encoding::toml` - TOML 1.0 parsing + emission.
//
//   - `parse(toml_text)`          -> Result<json::Value, errors::Error>
//   - `encode(value)`             -> Result<toml_text, errors::Error>
//   - `to_json_text(toml_text)`   -> Result<json_text, errors::Error>
//   - `from_json_text(json_text)` -> Result<toml_text, errors::Error>
//   - `is_valid(s)`               -> bool
//   - `pretty(s)`                 -> Result<toml_text, errors::Error>
//
// A document converts with each table's members by key, as `toml::Table`
// and a `json::Value` hold them. Typed `toml::decode::<T>` / `encode::<T>`
// go through the typed serde steps, which keep a struct's member order.

#![forbid(unsafe_code)]

use gossamer_runtime::codec::json_convert::{self, Members};

/// Parses `text` as TOML into the JSON value its tables read as.
pub fn parse(text: &str) -> Result<crate::json::Value, String> {
    crate::json::parse(&to_json_text(text)?).map_err(|e| e.to_string())
}

/// Encodes a JSON value whose top level is an object as a TOML document.
pub fn encode(value: &crate::json::Value) -> Result<String, String> {
    from_json_text(&crate::json::encode(value))
}

/// Parses `text` as TOML and returns its JSON text, each table's members
/// by key.
pub fn to_json_text(text: &str) -> Result<String, String> {
    json_convert::toml_to_json(text)
}

/// Renders a JSON document as TOML, each object's members by key. The top
/// level must be an object; arrays of objects become `[[name]]` table arrays.
pub fn from_json_text(json_text: &str) -> Result<String, String> {
    json_convert::to_toml(json_text, Members::Sorted)
}

/// `true` iff `text` parses as a TOML document.
#[must_use]
pub fn is_valid(text: &str) -> bool {
    toml::from_str::<toml::Value>(text).is_ok()
}

/// Round-trips `text` through the parser + pretty-printer.
pub fn pretty(text: &str) -> Result<String, String> {
    let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    toml::to_string_pretty(&value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_json_text_reads_tables_by_key() {
        let toml_text = r#"
            port = 8080
            host = "localhost"
            verbose = true
            when = 1979-05-27T07:32:00Z
        "#;
        let json = to_json_text(toml_text).unwrap();
        assert_eq!(
            json,
            r#"{"host":"localhost","port":8080,"verbose":true,"when":"1979-05-27T07:32:00Z"}"#
        );
    }

    #[test]
    fn is_valid_accepts_canonical() {
        assert!(is_valid("port = 8080"));
    }

    #[test]
    fn is_valid_rejects_garbage() {
        assert!(!is_valid("port == 8080"));
    }

    #[test]
    fn from_json_text_orders_members_by_key() {
        let json = r#"{"port":8080,"host":"localhost"}"#;
        assert_eq!(
            from_json_text(json).unwrap(),
            "host = \"localhost\"\nport = 8080\n"
        );
    }

    #[test]
    fn parse_and_encode_round_trip() {
        let value = parse("b = 1\na = [true]\n").unwrap();
        assert_eq!(crate::json::encode(&value), r#"{"a":[true],"b":1}"#);
        assert_eq!(encode(&value).unwrap(), "a = [true]\nb = 1\n");
    }

    #[test]
    fn pretty_normalizes() {
        let messy = "host =    \"localhost\"\nport=8080\n";
        let clean = pretty(messy).unwrap();
        assert!(clean.contains("host"));
        assert!(clean.contains("port"));
    }
}
