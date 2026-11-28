//! Plain-text templates.
//!
//! Source-compatible enough with Go's `text/template` for most simple
//! interpolation and conditional flows, without dragging in a full
//! action language. Supported syntax:
//!
//! - `{{ .field }}` - looks up `field` in the data map.
//! - `{{ .a.b }}` - chained map lookups.
//! - `{{ if .x }}…{{ else }}…{{ end }}` - boolean branch.
//! - `{{ range .items }}…{{ end }}` - iterate a sequence; inside the
//!   loop, `.` refers to the current item and parent fields are still
//!   reachable via stack-search.
//! - `{{ "literal" }}` - string literal.
//! - `{{- .x -}}` / `{{- if -}}` - strip surrounding whitespace.
//! - Comments: `{{/* anything */}}`.
//!
//! No custom function pipelines, no auto-escape (use
//! [`crate::html`] when emitting HTML).

#![forbid(unsafe_code)]
// The render loop walks one byte at a time over the template body,
// branching per `{{ }}` action shape; the flat match-on-action keeps
// the parser's intent in a single readable scan.
#![allow(
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::doc_markdown,
    clippy::missing_errors_doc
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use thiserror::Error;

/// Dynamic value passed into a template. Mirrors the JSON Value
/// shape so callers can plumb either source through unchanged.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `null` / missing-key sentinel.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit signed integer.
    Int(i64),
    /// 64-bit floating-point.
    Float(f64),
    /// UTF-8 string.
    String(String),
    /// Ordered sequence.
    Seq(Vec<Value>),
    /// Ordered map keyed by name.
    Map(BTreeMap<String, Value>),
}

impl Value {
    /// Owned string form (used by `{{ .x }}` rendering).
    #[must_use]
    pub fn to_text(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Int(n) => n.to_string(),
            Value::Float(n) => format!("{n}"),
            Value::String(s) => s.clone(),
            Value::Seq(items) => {
                let mut out = String::new();
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    out.push_str(&v.to_text());
                }
                out
            }
            Value::Map(_) => format!("{self:?}"),
        }
    }

    /// `if` truthiness, mirroring Go's template semantics.
    #[must_use]
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(n) => *n != 0,
            Value::Float(n) => *n != 0.0,
            Value::String(s) => !s.is_empty(),
            Value::Seq(items) => !items.is_empty(),
            Value::Map(m) => !m.is_empty(),
        }
    }

    /// Chases a `.a.b.c` lookup, `None` when a step names a key the value
    /// does not hold.
    #[must_use]
    pub fn find(&self, path: &str) -> Option<Value> {
        let mut cursor = self.clone();
        for segment in path.split('.').filter(|s| !s.is_empty()) {
            cursor = match cursor {
                Value::Map(m) => m.get(segment).cloned()?,
                _ => return None,
            };
        }
        Some(cursor)
    }

    /// Chases a `.a.b.c` lookup. Returns [`Value::Null`] for any
    /// missing intermediate.
    #[must_use]
    pub fn lookup(&self, path: &str) -> Value {
        let mut cursor = self.clone();
        for segment in path.split('.').filter(|s| !s.is_empty()) {
            cursor = match cursor {
                Value::Map(m) => m.get(segment).cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            };
        }
        cursor
    }
}

/// Parsed template AST.
#[derive(Debug, Clone)]
pub struct Template {
    nodes: Vec<Node>,
    /// Bodies bound by `{{define "name"}}`, reachable from anywhere in
    /// the template through `{{template "name"}}`.
    defines: std::collections::HashMap<String, Vec<Node>>,
}

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Substitute(Pipeline),
    If {
        cond: String,
        then_body: Vec<Node>,
        else_body: Vec<Node>,
    },
    Range {
        path: String,
        body: Vec<Node>,
    },
    /// `{{with .x}}..{{else}}..{{end}}` - renders the body with `.x` as
    /// the dot when it is truthy, the else branch otherwise.
    With {
        path: String,
        then_body: Vec<Node>,
        else_body: Vec<Node>,
    },
    /// `{{template "name"}}` / `{{template "name" .arg}}` - renders a
    /// template `{{define}}` bound, with the argument as its dot.
    Invoke {
        name: String,
        arg: Option<String>,
    },
}

/// One stage in a `{{ expr | f1 | f2 arg }}` pipeline.
#[derive(Debug, Clone)]
struct PipelineStage {
    func: String,
    args: Vec<String>,
}

/// Parsed pipeline: an initial field path, then zero-or-more
/// function applications. `{{ .name }}` is a pipeline with no
/// stages; `{{ .name | upper }}` has one stage.
#[derive(Debug, Clone)]
struct Pipeline {
    source: String,
    stages: Vec<PipelineStage>,
    /// The substitution asked to be written through unescaped, spelled
    /// `{{ safe .x }}` or `{{ .x | safe }}`. Only an escaping renderer
    /// acts on it; the text engine escapes nothing either way.
    raw: bool,
}

fn parse_pipeline(expr: &str) -> Pipeline {
    let mut parts = expr.split('|').map(str::trim);
    let mut source = parts.next().unwrap_or("").to_string();
    let mut raw = false;
    if let Some(rest) = source.strip_prefix("safe ") {
        raw = true;
        source = rest.trim().to_string();
    }
    let stages: Vec<PipelineStage> = parts
        .filter(|seg| {
            if seg.trim() == "safe" {
                raw = true;
                return false;
            }
            true
        })
        .map(|seg| {
            let mut tokens = seg.split_whitespace();
            let func = tokens.next().unwrap_or("").to_string();
            let args: Vec<String> = tokens.map(str::to_string).collect();
            PipelineStage { func, args }
        })
        .collect();
    Pipeline {
        source,
        stages,
        raw,
    }
}

/// Registry of named functions usable inside `{{ ... }}`
/// pipelines. Mirrors Go's `template.FuncMap`.
#[derive(Default)]
pub struct FuncMap {
    funcs: std::collections::HashMap<
        String,
        Arc<dyn Fn(&[Value]) -> Result<Value, Error> + Send + Sync>,
    >,
}

impl FuncMap {
    /// Empty FuncMap.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a function under `name`.
    pub fn add(
        &mut self,
        name: impl Into<String>,
        f: impl Fn(&[Value]) -> Result<Value, Error> + Send + Sync + 'static,
    ) -> &mut Self {
        self.funcs.insert(name.into(), Arc::new(f));
        self
    }

    /// Looks up a function.
    #[must_use]
    pub fn get(
        &self,
        name: &str,
    ) -> Option<Arc<dyn Fn(&[Value]) -> Result<Value, Error> + Send + Sync>> {
        self.funcs.get(name).cloned()
    }

    /// Default FuncMap with the common helpers
    /// (`upper`/`lower`/`trim`/`len`/`default`/`html_escape`).
    #[must_use]
    pub fn defaults() -> Self {
        let mut m = Self::new();
        m.add("upper", |args| match args.first() {
            Some(Value::String(s)) => Ok(Value::String(s.to_uppercase())),
            Some(v) => Ok(Value::String(v.to_text().to_uppercase())),
            None => Ok(Value::String(String::new())),
        });
        m.add("lower", |args| match args.first() {
            Some(Value::String(s)) => Ok(Value::String(s.to_lowercase())),
            Some(v) => Ok(Value::String(v.to_text().to_lowercase())),
            None => Ok(Value::String(String::new())),
        });
        m.add("trim", |args| match args.first() {
            Some(Value::String(s)) => Ok(Value::String(s.trim().to_string())),
            Some(v) => Ok(Value::String(v.to_text().trim().to_string())),
            None => Ok(Value::String(String::new())),
        });
        m.add("len", |args| match args.first() {
            Some(Value::String(s)) => Ok(Value::Int(s.chars().count() as i64)),
            Some(Value::Seq(items)) => Ok(Value::Int(items.len() as i64)),
            Some(Value::Map(map)) => Ok(Value::Int(map.len() as i64)),
            _ => Ok(Value::Int(0)),
        });
        m.add("default", |args| {
            // `{{ .x | default "fallback" }}` - return arg if .x
            // is falsy / missing, else .x.
            let value = args.first().cloned().unwrap_or(Value::Null);
            let fallback = args.get(1).cloned().unwrap_or(Value::Null);
            if value.truthy() {
                Ok(value)
            } else {
                Ok(fallback)
            }
        });
        m.add("html_escape", |args| {
            let s = args.first().map(Value::to_text).unwrap_or_default();
            let mut out = String::with_capacity(s.len());
            for ch in s.chars() {
                match ch {
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    '&' => out.push_str("&amp;"),
                    '"' => out.push_str("&quot;"),
                    '\'' => out.push_str("&#39;"),
                    c => out.push(c),
                }
            }
            Ok(Value::String(out))
        });
        m
    }
}

impl std::fmt::Debug for FuncMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuncMap")
            .field("names", &self.funcs.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Errors raised by the parser / renderer.
#[derive(Debug, Clone, Error)]
pub enum Error {
    /// Source did not parse.
    #[error("template: {0}")]
    Parse(String),
    /// `{{ end }}` arrived without a matching opener (or vice versa).
    #[error("template: unbalanced action: {0}")]
    Unbalanced(String),
    /// A parse error at a place in the source, counted from 1.
    #[error("template:{line}:{column}: {message}")]
    At {
        /// The line of the action that failed.
        line: usize,
        /// The column, in characters, where that action opens.
        column: usize,
        /// What was wrong there.
        message: String,
    },
    /// A field or key the template names is not in the data, under
    /// [`MissingKey::Error`].
    #[error("template: {0}: the data has no such field or key")]
    Missing(String),
}

/// What a field or key the data does not hold renders as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MissingKey {
    /// An error naming the path, so a misspelt field is caught.
    Error,
    /// The empty value: nothing in text, false in a condition.
    #[default]
    Empty,
}

/// Parses a template source.
pub fn parse(source: &str) -> Result<Template, Error> {
    let tokens = tokenize(source);
    let mut iter = tokens.into_iter().peekable();
    let mut defines = std::collections::HashMap::new();
    let nodes =
        parse_block(&mut iter, false, &mut defines).map_err(|(at, e)| locate(source, at, e))?;
    if let Some(Token::Action { at, .. }) = iter.peek() {
        return Err(locate(
            source,
            *at,
            Error::Unbalanced("trailing tokens".into()),
        ));
    }
    Ok(Template { nodes, defines })
}

/// `error` placed at byte `at` of `source`, by line and character column.
fn locate(source: &str, at: usize, error: Error) -> Error {
    let before = &source[..at.min(source.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    let message = match error {
        Error::Parse(m) => m,
        Error::Unbalanced(m) => format!("unbalanced action: {m}"),
        other => other.to_string(),
    };
    Error::At {
        line,
        column,
        message,
    }
}

impl Template {
    /// Parses `source` directly, returning a [`Template`].
    pub fn from_source(source: &str) -> Result<Self, Error> {
        parse(source)
    }

    /// Renders the template against `data` with no FuncMap.
    pub fn render(&self, data: &Value) -> Result<String, Error> {
        let empty = FuncMap::new();
        self.render_with_funcs(data, &empty)
    }

    /// Renders the template against `data`, resolving pipeline
    /// function calls through `funcs`. Functions not in
    /// `funcs` fall through to the built-in set
    /// ([`FuncMap::defaults`]). Use [`FuncMap::defaults`] +
    /// [`FuncMap::add`] to extend with custom helpers.
    pub fn render_with_funcs(&self, data: &Value, funcs: &FuncMap) -> Result<String, Error> {
        self.render_with(data, funcs, None)
    }

    /// Renders with an optional [`ValueEscaper`] applied to every
    /// substituted value. The HTML engine renders this AST with one, so a
    /// value inside `if`, `range`, `with`, or `template` is escaped by the
    /// context it lands in.
    pub fn render_with(
        &self,
        data: &Value,
        funcs: &FuncMap,
        escaper: Option<ValueEscaper<'_>>,
    ) -> Result<String, Error> {
        self.render_with_missing(data, funcs, escaper, MissingKey::Empty)
    }

    /// [`Self::render_with`], with what a field the data does not hold
    /// renders as.
    pub fn render_with_missing(
        &self,
        data: &Value,
        funcs: &FuncMap,
        escaper: Option<ValueEscaper<'_>>,
        missing: MissingKey,
    ) -> Result<String, Error> {
        let defaults = FuncMap::defaults();
        let ctx = RenderCtx {
            funcs,
            defaults: &defaults,
            defines: &self.defines,
            escaper,
            missing,
        };
        let mut sink = Sink {
            out: String::with_capacity(64),
            literal: String::new(),
        };
        render_nodes(&self.nodes, std::slice::from_ref(data), &ctx, &mut sink)?;
        Ok(sink.out)
    }
}

/// Renders `source` against a data context given as JSON text: objects as
/// maps, arrays as sequences, numbers, strings, booleans, and `null`. No
/// escaping is applied, so this is for configuration, code, and mail text,
/// not HTML.
///
/// # Errors
/// JSON text that does not parse, or a template that does not.
pub fn render_json(source: &str, json_data: &str) -> Result<String, Error> {
    render(source, &json_to_value(json_data)?)
}

/// The template value the JSON `text` describes; empty text is `null`.
///
/// # Errors
/// The text is not JSON.
pub fn json_to_value(text: &str) -> Result<Value, Error> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| Error::Parse(format!("template data: {e}")))?;
    Ok(json_value(&v))
}

fn json_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0)), Value::Int),
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(items) => Value::Seq(items.iter().map(json_value).collect()),
        serde_json::Value::Object(map) => {
            let mut m = std::collections::BTreeMap::new();
            for (k, val) in map {
                m.insert(k.clone(), json_value(val));
            }
            Value::Map(m)
        }
    }
}

/// Convenience renderer with no custom funcs.
pub fn render(source: &str, data: &Value) -> Result<String, Error> {
    let tpl = parse(source)?;
    tpl.render(data)
}

/// Convenience renderer with a [`FuncMap`].
pub fn render_with_funcs(source: &str, data: &Value, funcs: &FuncMap) -> Result<String, Error> {
    let tpl = parse(source)?;
    tpl.render_with_funcs(data, funcs)
}

/// Writes the rendered template into `out`. Useful for streaming.
pub fn render_to(template: &Template, data: &Value, out: &mut String) -> Result<(), Error> {
    let empty = FuncMap::new();
    out.push_str(&template.render_with(data, &empty, None)?);
    Ok(())
}

#[derive(Debug, Clone)]
enum Token {
    Text(String),
    Action {
        body: String,
        trim_left: bool,
        trim_right: bool,
        /// Byte offset of the action's `{{`.
        at: usize,
    },
}

fn tokenize(source: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let bytes = source.as_bytes();
    let mut cursor = 0;
    let mut text_start = 0;
    while cursor < bytes.len() {
        if cursor + 1 < bytes.len() && bytes[cursor] == b'{' && bytes[cursor + 1] == b'{' {
            let action_start = cursor;
            if cursor > text_start {
                tokens.push(Token::Text(
                    String::from_utf8_lossy(&bytes[text_start..cursor]).into_owned(),
                ));
            }
            let mut body_start = cursor + 2;
            let mut trim_left = false;
            if body_start < bytes.len() && bytes[body_start] == b'-' {
                trim_left = true;
                body_start += 1;
                if body_start < bytes.len() && bytes[body_start].is_ascii_whitespace() {
                    body_start += 1;
                }
            }
            let mut end = body_start;
            while end + 1 < bytes.len() {
                if bytes[end] == b'}' && bytes[end + 1] == b'}' {
                    break;
                }
                if end + 2 < bytes.len()
                    && bytes[end] == b'-'
                    && bytes[end + 1] == b'}'
                    && bytes[end + 2] == b'}'
                {
                    break;
                }
                end += 1;
            }
            let mut trim_right = false;
            let mut body_end = end;
            if end + 2 < bytes.len()
                && bytes[end] == b'-'
                && bytes[end + 1] == b'}'
                && bytes[end + 2] == b'}'
            {
                trim_right = true;
                if body_end > body_start && bytes[body_end - 1].is_ascii_whitespace() {
                    body_end -= 1;
                }
                cursor = end + 3;
            } else if end + 1 < bytes.len() && bytes[end] == b'}' && bytes[end + 1] == b'}' {
                cursor = end + 2;
            } else {
                tokens.push(Token::Text(
                    String::from_utf8_lossy(&bytes[text_start..]).into_owned(),
                ));
                return tokens;
            }
            let body = String::from_utf8_lossy(&bytes[body_start..body_end])
                .trim()
                .to_string();
            tokens.push(Token::Action {
                body,
                trim_left,
                trim_right,
                at: action_start,
            });
            text_start = cursor;
        } else {
            cursor += 1;
        }
    }
    if text_start < bytes.len() {
        tokens.push(Token::Text(
            String::from_utf8_lossy(&bytes[text_start..]).into_owned(),
        ));
    }
    apply_trims(&mut tokens);
    tokens
}

fn apply_trims(tokens: &mut [Token]) {
    let mut i = 0;
    while i < tokens.len() {
        let (trim_left, trim_right) = if let Token::Action {
            trim_left,
            trim_right,
            ..
        } = &tokens[i]
        {
            (*trim_left, *trim_right)
        } else {
            (false, false)
        };
        if trim_left && i > 0 {
            if let Token::Text(t) = &mut tokens[i - 1] {
                while t.ends_with(|c: char| c.is_whitespace()) {
                    t.pop();
                }
            }
        }
        if trim_right && i + 1 < tokens.len() {
            if let Token::Text(t) = &mut tokens[i + 1] {
                let trimmed = t
                    .trim_start_matches(|c: char| c.is_whitespace())
                    .to_string();
                *t = trimmed;
            }
        }
        i += 1;
    }
}

fn parse_block<I>(
    tokens: &mut std::iter::Peekable<I>,
    inside_block: bool,
    defines: &mut std::collections::HashMap<String, Vec<Node>>,
) -> Result<Vec<Node>, (usize, Error)>
where
    I: Iterator<Item = Token>,
{
    let mut nodes = Vec::new();
    while let Some(tok) = tokens.peek() {
        match tok {
            Token::Text(_) => {
                if let Some(Token::Text(t)) = tokens.next() {
                    nodes.push(Node::Text(t));
                }
            }
            Token::Action { body, at, .. } => {
                let body = body.clone();
                let at = *at;
                let trimmed = body.trim();
                if trimmed == "end" || trimmed == "else" {
                    if !inside_block {
                        return Err((at, Error::Unbalanced(trimmed.to_string())));
                    }
                    return Ok(nodes);
                }
                tokens.next();
                if trimmed.starts_with("/*") && trimmed.ends_with("*/") {
                    continue;
                }
                if let Some(rest) = trimmed.strip_prefix("if ") {
                    let cond = rest.trim().to_string();
                    let then_body = parse_block(tokens, true, defines)?;
                    let mut else_body = Vec::new();
                    if let Some(Token::Action { body, .. }) = tokens.peek() {
                        if body.trim() == "else" {
                            tokens.next();
                            else_body = parse_block(tokens, true, defines)?;
                        }
                    }
                    let closer = tokens.next();
                    match closer {
                        Some(Token::Action { body, .. }) if body.trim() == "end" => {}
                        _ => return Err((at, Error::Unbalanced("expected end after if".into()))),
                    }
                    nodes.push(Node::If {
                        cond,
                        then_body,
                        else_body,
                    });
                } else if let Some(rest) = trimmed.strip_prefix("range ") {
                    let path = rest.trim().to_string();
                    let body = parse_block(tokens, true, defines)?;
                    let closer = tokens.next();
                    match closer {
                        Some(Token::Action { body, .. }) if body.trim() == "end" => {}
                        _ => {
                            return Err((at, Error::Unbalanced("expected end after range".into())));
                        }
                    }
                    nodes.push(Node::Range { path, body });
                } else if let Some(rest) = trimmed.strip_prefix("with ") {
                    let path = rest.trim().to_string();
                    let then_body = parse_block(tokens, true, defines)?;
                    let mut else_body = Vec::new();
                    if let Some(Token::Action { body, .. }) = tokens.peek() {
                        if body.trim() == "else" {
                            tokens.next();
                            else_body = parse_block(tokens, true, defines)?;
                        }
                    }
                    match tokens.next() {
                        Some(Token::Action { body, .. }) if body.trim() == "end" => {}
                        _ => return Err((at, Error::Unbalanced("expected end after with".into()))),
                    }
                    nodes.push(Node::With {
                        path,
                        then_body,
                        else_body,
                    });
                } else if let Some(rest) = trimmed.strip_prefix("define ") {
                    let name = unquote(rest.trim());
                    let body = parse_block(tokens, true, defines)?;
                    match tokens.next() {
                        Some(Token::Action { body, .. }) if body.trim() == "end" => {}
                        _ => {
                            return Err((
                                at,
                                Error::Unbalanced("expected end after define".into()),
                            ));
                        }
                    }
                    // A definition contributes no output where it is
                    // written; it is reachable by name from anywhere.
                    defines.insert(name, body);
                } else if let Some(rest) = trimmed.strip_prefix("template ") {
                    let mut parts = rest.trim().splitn(2, char::is_whitespace);
                    let name = unquote(parts.next().unwrap_or("").trim());
                    let arg = parts
                        .next()
                        .map(str::trim)
                        .filter(|a| !a.is_empty())
                        .map(str::to_string);
                    nodes.push(Node::Invoke { name, arg });
                } else if trimmed == "block" || trimmed.starts_with("block ") {
                    return Err((
                        at,
                        Error::Unbalanced(
                            "block is not supported; write define plus template".into(),
                        ),
                    ));
                } else {
                    nodes.push(Node::Substitute(parse_pipeline(trimmed)));
                }
            }
        }
    }
    Ok(nodes)
}

/// Strips one layer of matching double quotes, for a `{{define "x"}}` or
/// `{{template "x"}}` name.
fn unquote(text: &str) -> String {
    text.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(text)
        .to_string()
}

fn resolve(stack: &[Value], expr: &str, missing: MissingKey) -> Result<Value, Error> {
    let expr = expr.trim();
    if expr.starts_with('"') && expr.ends_with('"') && expr.len() >= 2 {
        return Ok(Value::String(expr[1..expr.len() - 1].to_string()));
    }
    if let Ok(n) = expr.parse::<i64>() {
        return Ok(Value::Int(n));
    }
    if expr == "." {
        return Ok(stack.last().cloned().unwrap_or(Value::Null));
    }
    if let Some(rest) = expr.strip_prefix('.') {
        // The innermost frame that holds the path answers; a `null` there
        // lets an outer frame's value through, as an empty field does.
        let mut held = false;
        for frame in stack.iter().rev() {
            match frame.find(rest) {
                Some(Value::Null) => held = true,
                Some(value) => return Ok(value),
                None => {}
            }
        }
        return match (held, missing) {
            (false, MissingKey::Error) => Err(Error::Missing(expr.to_string())),
            _ => Ok(Value::Null),
        };
    }
    Ok(Value::Null)
}

/// Transforms one substituted value before it is written out.
///
/// The first argument is the literal template text emitted so far, which
/// is what decides the context the value lands in - HTML text, an
/// attribute, a URL, a script. Literal text only: an already-escaped
/// value cannot change the context of the one after it, and including it
/// would let data steer the classifier.
///
/// The third argument is whether the substitution asked to be written
/// through unescaped (`{{ safe .x }}`).
///
/// The HTML engine supplies one so `if`, `range`, `with`, and `template`
/// all escape by context; the text engine supplies none and writes the
/// value through.
pub type ValueEscaper<'a> = &'a dyn Fn(&str, &str, bool) -> String;

/// Everything a render pass carries that does not change as it descends.
struct RenderCtx<'a> {
    funcs: &'a FuncMap,
    defaults: &'a FuncMap,
    defines: &'a std::collections::HashMap<String, Vec<Node>>,
    escaper: Option<ValueEscaper<'a>>,
    missing: MissingKey,
}

/// Output being assembled, plus the literal text an escaper classifies by.
struct Sink {
    out: String,
    literal: String,
}

fn render_nodes(
    nodes: &[Node],
    stack: &[Value],
    ctx: &RenderCtx<'_>,
    sink: &mut Sink,
) -> Result<(), Error> {
    for node in nodes {
        match node {
            Node::Text(t) => {
                sink.out.push_str(t);
                sink.literal.push_str(t);
            }
            Node::Substitute(pipe) => {
                let value = apply_pipeline(stack, pipe, ctx)?;
                let text = value.to_text();
                match ctx.escaper {
                    Some(escape) => sink.out.push_str(&escape(&sink.literal, &text, pipe.raw)),
                    None => sink.out.push_str(&text),
                }
            }
            Node::If {
                cond,
                then_body,
                else_body,
            } => {
                let value = resolve(stack, cond, ctx.missing)?;
                if value.truthy() {
                    render_nodes(then_body, stack, ctx, sink)?;
                } else {
                    render_nodes(else_body, stack, ctx, sink)?;
                }
            }
            Node::With {
                path,
                then_body,
                else_body,
            } => {
                let value = resolve(stack, path, ctx.missing)?;
                if value.truthy() {
                    let mut child_stack: Vec<Value> = stack.to_vec();
                    child_stack.push(value);
                    render_nodes(then_body, &child_stack, ctx, sink)?;
                } else {
                    render_nodes(else_body, stack, ctx, sink)?;
                }
            }
            Node::Invoke { name, arg } => {
                let Some(body) = ctx.defines.get(name) else {
                    return Err(Error::Parse(format!("template {name:?} is not defined")));
                };
                match arg {
                    Some(expr) => {
                        let mut child_stack: Vec<Value> = stack.to_vec();
                        child_stack.push(resolve(stack, expr, ctx.missing)?);
                        render_nodes(body, &child_stack, ctx, sink)?;
                    }
                    None => render_nodes(body, stack, ctx, sink)?,
                }
            }
            Node::Range { path, body } => {
                let value = resolve(stack, path, ctx.missing)?;
                if let Value::Seq(items) = value {
                    for item in items {
                        let mut child_stack: Vec<Value> = stack.to_vec();
                        child_stack.push(item);
                        render_nodes(body, &child_stack, ctx, sink)?;
                    }
                } else if let Value::Map(map) = value {
                    for (k, v) in map {
                        let mut framed = BTreeMap::new();
                        framed.insert("Key".to_string(), Value::String(k));
                        framed.insert("Value".to_string(), v);
                        let mut child_stack: Vec<Value> = stack.to_vec();
                        child_stack.push(Value::Map(framed));
                        render_nodes(body, &child_stack, ctx, sink)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn apply_pipeline(stack: &[Value], pipe: &Pipeline, ctx: &RenderCtx<'_>) -> Result<Value, Error> {
    let (funcs, defaults) = (ctx.funcs, ctx.defaults);
    let mut value = resolve_source(stack, &pipe.source, ctx)?;
    for stage in &pipe.stages {
        let f = lookup_func(stage.func.as_str(), funcs, defaults)?;
        let mut args: Vec<Value> = Vec::with_capacity(1 + stage.args.len());
        args.push(value);
        for a in &stage.args {
            args.push(resolve(stack, a, ctx.missing)?);
        }
        value = f(&args)?;
    }
    Ok(value)
}

/// Resolves a pipeline's head.
///
/// A head is normally a field path or a literal. Go also spells a call
/// prefix-first - `{{ len .items }}` - so a head whose first word names a
/// function is that call over the words after it. `{{ .items | len }}` is
/// the same call written pipe-first.
fn resolve_source(stack: &[Value], source: &str, ctx: &RenderCtx<'_>) -> Result<Value, Error> {
    let source = source.trim();
    let Some((head, rest)) = source.split_once(char::is_whitespace) else {
        return resolve(stack, source, ctx.missing);
    };
    let rest = rest.trim();
    if rest.is_empty() || head.starts_with('.') || head.starts_with('"') {
        return resolve(stack, source, ctx.missing);
    }
    let Ok(f) = lookup_func(head, ctx.funcs, ctx.defaults) else {
        return resolve(stack, source, ctx.missing);
    };
    let args = rest
        .split_whitespace()
        .map(|a| resolve(stack, a, ctx.missing))
        .collect::<Result<Vec<Value>, Error>>()?;
    f(&args)
}

fn lookup_func(
    name: &str,
    funcs: &FuncMap,
    defaults: &FuncMap,
) -> Result<Arc<dyn Fn(&[Value]) -> Result<Value, Error> + Send + Sync>, Error> {
    if let Some(f) = funcs.get(name) {
        return Ok(f);
    }
    defaults
        .get(name)
        .ok_or_else(|| Error::Parse(format!("unknown template function {name:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, Value)]) -> Value {
        let mut m = BTreeMap::new();
        for (k, v) in entries {
            m.insert((*k).to_string(), v.clone());
        }
        Value::Map(m)
    }

    #[test]
    fn renders_plain_text() {
        let out = render("hello world", &Value::Null).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn substitutes_field() {
        let data = map(&[("name", Value::String("gossamer".into()))]);
        let out = render("hello {{ .name }}", &data).unwrap();
        assert_eq!(out, "hello gossamer");
    }

    #[test]
    fn handles_if_else() {
        let yes = map(&[("ok", Value::Bool(true))]);
        let no = map(&[("ok", Value::Bool(false))]);
        let tpl = "{{ if .ok }}YES{{ else }}NO{{ end }}";
        assert_eq!(render(tpl, &yes).unwrap(), "YES");
        assert_eq!(render(tpl, &no).unwrap(), "NO");
    }

    #[test]
    fn handles_range_over_seq() {
        let data = map(&[(
            "items",
            Value::Seq(vec![
                Value::String("a".into()),
                Value::String("b".into()),
                Value::String("c".into()),
            ]),
        )]);
        let tpl = "{{ range .items }}[{{ . }}]{{ end }}";
        assert_eq!(render(tpl, &data).unwrap(), "[a][b][c]");
    }

    #[test]
    fn whitespace_trim_works() {
        let data = map(&[("x", Value::Int(42))]);
        let tpl = "before  {{- .x -}}  after";
        assert_eq!(render(tpl, &data).unwrap(), "before42after");
    }

    #[test]
    fn comments_are_dropped() {
        let out = render("a{{/* comment */}}b", &Value::Null).unwrap();
        assert_eq!(out, "ab");
    }

    #[test]
    fn nested_lookup() {
        let inner = map(&[("name", Value::String("nested".into()))]);
        let outer = map(&[("user", inner)]);
        let out = render("hi {{ .user.name }}", &outer).unwrap();
        assert_eq!(out, "hi nested");
    }

    // --- P1 template parity: FuncMap + pipelines ----------------

    #[test]
    fn pipeline_uppercase_works() {
        let data = map(&[("name", Value::String("ada".into()))]);
        let out = render("hi {{ .name | upper }}", &data).unwrap();
        assert_eq!(out, "hi ADA");
    }

    #[test]
    fn pipeline_chained_functions() {
        let data = map(&[("name", Value::String("  Ada  ".into()))]);
        let out = render("[{{ .name | trim | lower }}]", &data).unwrap();
        assert_eq!(out, "[ada]");
    }

    #[test]
    fn pipeline_default_falls_back_when_empty() {
        let data = map(&[("name", Value::String(String::new()))]);
        let out = render("hi {{ .name | default \"anon\" }}", &data).unwrap();
        // Note: default's second arg "anon" is a literal string.
        assert_eq!(out, "hi anon");
    }

    #[test]
    fn pipeline_len_counts_seq() {
        let data = map(&[(
            "items",
            Value::Seq(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        )]);
        let out = render("{{ .items | len }}", &data).unwrap();
        assert_eq!(out, "3");
    }

    #[test]
    fn pipeline_html_escape_protects_metachars() {
        let data = map(&[("body", Value::String("<b>hi</b>".into()))]);
        let out = render("{{ .body | html_escape }}", &data).unwrap();
        assert_eq!(out, "&lt;b&gt;hi&lt;/b&gt;");
    }

    #[test]
    fn custom_func_via_funcmap() {
        let mut fm = FuncMap::new();
        fm.add("exclaim", |args| {
            let s = args.first().map(Value::to_text).unwrap_or_default();
            Ok(Value::String(format!("{s}!")))
        });
        let tpl = parse("{{ .name | exclaim }}").unwrap();
        let data = map(&[("name", Value::String("hi".into()))]);
        let out = tpl.render_with_funcs(&data, &fm).unwrap();
        assert_eq!(out, "hi!");
    }

    #[test]
    fn unknown_function_errors() {
        let tpl = parse("{{ .x | not_a_real_func }}").unwrap();
        let data = map(&[("x", Value::String("v".into()))]);
        let err = tpl.render(&data).unwrap_err();
        assert!(matches!(err, Error::Parse(_)));
    }

    #[test]
    fn pipeline_passes_through_when_no_stages() {
        // Just a substitution - no pipeline functions.
        let data = map(&[("x", Value::String("plain".into()))]);
        let out = render("{{ .x }}", &data).unwrap();
        assert_eq!(out, "plain");
    }

    #[test]
    fn render_with_funcs_helper_works() {
        let mut fm = FuncMap::new();
        fm.add("double", |args| match args.first() {
            Some(Value::Int(n)) => Ok(Value::Int(n * 2)),
            _ => Ok(Value::Null),
        });
        let data = map(&[("n", Value::Int(21))]);
        let out = render_with_funcs("{{ .n | double }}", &data, &fm).unwrap();
        assert_eq!(out, "42");
    }
}
