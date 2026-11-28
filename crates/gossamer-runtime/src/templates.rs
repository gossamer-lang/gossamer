//! Parsed `std::text::template` and `std::html::template` templates on every
//! tier. A `template::Template` value holds an id into this registry, so a
//! template parses once and renders any number of times, from any goroutine;
//! its last reference gives the entry back.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use gossamer_template::text::MissingKey;
use parking_lot::RwLock;

enum Parsed {
    Text(gossamer_template::text::Template),
    Html(gossamer_template::html::Template),
}

struct Entry {
    parsed: Parsed,
    missing: MissingKey,
}

static TEMPLATES: RwLock<Option<HashMap<i64, Arc<Entry>>>> = RwLock::new(None);
static NEXT_TEMPLATE: AtomicI64 = AtomicI64::new(1);

fn parse(source: &str, html: bool) -> Result<Parsed, String> {
    if html {
        gossamer_template::html::parse(source).map(Parsed::Html)
    } else {
        gossamer_template::text::parse(source).map(Parsed::Text)
    }
    .map_err(|e| e.to_string())
}

fn render(entry: &Entry, json_text: &str) -> Result<String, String> {
    let data = gossamer_template::text::json_to_value(json_text).map_err(|e| e.to_string())?;
    match &entry.parsed {
        Parsed::Text(t) => t.render_with_missing(
            &data,
            &gossamer_template::text::FuncMap::new(),
            None,
            entry.missing,
        ),
        Parsed::Html(t) => t.render_with_missing(&data, entry.missing),
    }
    .map_err(|e| e.to_string())
}

fn missing_key(strict: bool) -> MissingKey {
    if strict {
        MissingKey::Error
    } else {
        MissingKey::Empty
    }
}

/// Parses `source` (escaping by HTML context when `html`) and answers the id
/// of the parsed template. Under `strict`, a field the data does not hold is
/// an error when the template renders.
///
/// # Errors
/// The source does not parse; the message names the line and column.
pub fn new_template(source: &str, html: bool, strict: bool) -> Result<i64, String> {
    let entry = Entry {
        parsed: parse(source, html)?,
        missing: missing_key(strict),
    };
    let id = NEXT_TEMPLATE.fetch_add(1, Ordering::Relaxed);
    TEMPLATES
        .write()
        .get_or_insert_with(HashMap::new)
        .insert(id, Arc::new(entry));
    Ok(id)
}

/// Renders the template `id` against the JSON text `json_text`.
///
/// # Errors
/// `id` names no live template, the JSON does not parse, or the template
/// names a field the data does not hold under `MissingKey::Error`.
pub fn render_template(id: i64, json_text: &str) -> Result<String, String> {
    let entry = TEMPLATES
        .read()
        .as_ref()
        .and_then(|templates| templates.get(&id).cloned())
        .ok_or_else(|| "template: the template was released".to_string())?;
    render(&entry, json_text)
}

/// Parses `source` and renders it once against `json_text`, a field the data
/// does not hold being an error.
///
/// # Errors
/// As [`new_template`] and [`render_template`].
pub fn render_source(source: &str, html: bool, json_text: &str) -> Result<String, String> {
    let entry = Entry {
        parsed: parse(source, html)?,
        missing: MissingKey::Error,
    };
    render(&entry, json_text)
}

/// Gives back the template `id`; a later release of it does nothing.
pub fn release_template(id: i64) {
    if let Some(templates) = TEMPLATES.write().as_mut() {
        templates.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_template_parses_once_and_renders_each_call() {
        let id = new_template("hi {{ .name }}", false, true).unwrap();
        assert_eq!(render_template(id, r#"{"name": "a"}"#).unwrap(), "hi a");
        assert_eq!(render_template(id, r#"{"name": "b"}"#).unwrap(), "hi b");
        release_template(id);
        assert!(render_template(id, "{}").is_err());
    }

    #[test]
    fn a_missing_field_is_an_error_naming_it_unless_empty_is_chosen() {
        let strict = new_template("{{ .user.email }}", false, true).unwrap();
        let error = render_template(strict, r#"{"user": {}}"#).unwrap_err();
        assert!(error.contains(".user.email"), "{error}");
        let lenient = new_template("[{{ .user.email }}]", false, false).unwrap();
        assert_eq!(render_template(lenient, r#"{"user": {}}"#).unwrap(), "[]");
        let null = new_template("[{{ .x }}]", false, true).unwrap();
        assert_eq!(render_template(null, r#"{"x": null}"#).unwrap(), "[]");
    }

    #[test]
    fn a_syntax_error_names_its_line_and_column() {
        let error = new_template("one\n  {{ if .x }}two", false, true).unwrap_err();
        assert!(error.starts_with("template:2:3: "), "{error}");
    }

    #[test]
    fn html_templates_escape_by_context() {
        let id = new_template("<p>{{ .x }}</p>", true, true).unwrap();
        assert_eq!(
            render_template(id, r#"{"x": "<b>"}"#).unwrap(),
            "<p>&lt;b&gt;</p>"
        );
    }
}
