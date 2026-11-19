//! The serde attributes a struct, enum, variant, or field may carry, read the
//! same way by the frontend that checks them and the pass that writes the
//! codecs.
//!
//! - On a struct or enum: `#[rename_all("camelCase")]` respells every field
//!   (struct) or variant (enum) that has no `#[rename]` of its own, and
//!   `#[tag("type")]` (enum only) writes the variant name as that member of
//!   the variant's own object instead of as a one-key wrapper.
//! - On a variant: `#[rename("name")]`.
//! - On a field (of a struct or a struct variant): `#[rename("key")]`,
//!   `#[skip]`, `#[default]`, and `#[default(value)]`.

use crate::items::Attrs;

/// How `#[rename_all]` respells a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameRule {
    /// `lowercase`: words joined, all lower case.
    Lower,
    /// `UPPERCASE`: words joined, all upper case.
    Upper,
    /// `camelCase`.
    Camel,
    /// `PascalCase`.
    Pascal,
    /// `snake_case`.
    Snake,
    /// `SCREAMING_SNAKE_CASE`.
    ScreamingSnake,
    /// `kebab-case`.
    Kebab,
    /// `SCREAMING-KEBAB-CASE`.
    ScreamingKebab,
}

impl RenameRule {
    /// Every rule by the spelling `#[rename_all]` takes.
    pub const ALL: [(&'static str, Self); 8] = [
        ("lowercase", Self::Lower),
        ("UPPERCASE", Self::Upper),
        ("camelCase", Self::Camel),
        ("PascalCase", Self::Pascal),
        ("snake_case", Self::Snake),
        ("SCREAMING_SNAKE_CASE", Self::ScreamingSnake),
        ("kebab-case", Self::Kebab),
        ("SCREAMING-KEBAB-CASE", Self::ScreamingKebab),
    ];

    /// The rule a `#[rename_all]` argument names.
    #[must_use]
    pub fn named(text: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find_map(|(name, rule)| (*name == text).then_some(*rule))
    }

    /// `name` (a `snake_case` field or a `PascalCase` variant) respelled.
    #[must_use]
    pub fn apply(self, name: &str) -> String {
        let words = words(name);
        let capitalized = |w: &str| {
            let mut chars = w.chars();
            chars.next().map_or_else(String::new, |first| {
                first
                    .to_uppercase()
                    .chain(chars.flat_map(char::to_lowercase))
                    .collect()
            })
        };
        match self {
            Self::Lower => words.concat().to_lowercase(),
            Self::Upper => words.concat().to_uppercase(),
            Self::Snake => words.join("_").to_lowercase(),
            Self::ScreamingSnake => words.join("_").to_uppercase(),
            Self::Kebab => words.join("-").to_lowercase(),
            Self::ScreamingKebab => words.join("-").to_uppercase(),
            Self::Pascal => words.iter().map(|w| capitalized(w)).collect(),
            Self::Camel => words
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    if i == 0 {
                        w.to_lowercase()
                    } else {
                        capitalized(w)
                    }
                })
                .collect(),
        }
    }
}

/// The words of an identifier: split at `_` and before an upper-case letter
/// that follows a lower-case letter or digit.
fn words(name: &str) -> Vec<&str> {
    let mut words = Vec::new();
    for part in name.split('_').filter(|p| !p.is_empty()) {
        let mut start = 0;
        let mut prev: Option<char> = None;
        for (at, c) in part.char_indices() {
            if c.is_uppercase()
                && prev.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit())
                && at > start
            {
                words.push(&part[start..at]);
                start = at;
            }
            prev = Some(c);
        }
        words.push(&part[start..]);
    }
    words
}

/// A value `#[default(value)]` names: a literal, `None`, or a unit variant of
/// the field's enum type (by its name, qualified or not).
#[derive(Debug, Clone, PartialEq)]
pub enum DefaultValue {
    /// An integer literal, as written without `_`.
    Int(String),
    /// A float literal, as written without `_`.
    Float(String),
    /// `true` or `false`.
    Bool(bool),
    /// A string literal's text.
    Str(String),
    /// A `char` literal.
    Char(char),
    /// `None`.
    None,
    /// The last segment of a path to a unit variant.
    Variant(String),
}

impl DefaultValue {
    /// The value an attribute's argument text spells, or `None` when it is
    /// not one of the accepted forms.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        match text {
            "true" => return Some(Self::Bool(true)),
            "false" => return Some(Self::Bool(false)),
            "None" => return Some(Self::None),
            _ => {}
        }
        if let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
            return unescape(inner).map(Self::Str);
        }
        if let Some(inner) = text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
            let decoded = unescape(inner)?;
            let mut chars = decoded.chars();
            return match (chars.next(), chars.next()) {
                (Some(c), None) => Some(Self::Char(c)),
                _ => None,
            };
        }
        let digits = text.replace('_', "");
        let unsigned = digits.strip_prefix('-').unwrap_or(&digits);
        if unsigned.starts_with(|c: char| c.is_ascii_digit()) {
            if digits.parse::<i64>().is_ok() || digits.parse::<u64>().is_ok() {
                return Some(Self::Int(digits));
            }
            if digits.parse::<f64>().is_ok() && !digits.ends_with('.') {
                return Some(Self::Float(digits));
            }
            return None;
        }
        let segments: Vec<&str> = text.split("::").map(str::trim).collect();
        let is_ident = |s: &&str| {
            s.chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
                && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        };
        if segments.iter().all(is_ident) {
            return segments
                .last()
                .map(|last| Self::Variant((*last).to_string()));
        }
        None
    }
}

fn unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            '0' => '\0',
            '\\' => '\\',
            '"' => '"',
            '\'' => '\'',
            _ => return None,
        });
    }
    Some(out)
}

/// What a missing field reads as.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldDefault {
    /// `#[default]`: the type's zero value (a user type's `default()`).
    Zero,
    /// `#[default(value)]`.
    Value(DefaultValue),
    /// The default the field declares (`name: T = expr`), evaluated by the
    /// function of this name the parser wrote beside the type.
    Declared(String),
}

/// A field's serde attributes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SerdeField {
    /// `#[rename("key")]`.
    pub rename: Option<String>,
    /// `#[skip]`: never written, and read as its default.
    pub skip: bool,
    /// `#[default]` or `#[default(value)]`.
    pub default: Option<FieldDefault>,
}

/// A struct's or enum's serde attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SerdeContainer {
    /// `#[rename_all("rule")]`.
    pub rename_all: Option<RenameRule>,
    /// `#[tag("member")]`.
    pub tag: Option<String>,
}

/// A misspelled or misplaced serde attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SerdeAttrProblem {
    /// The attribute needs a quoted string argument (`rename`, `tag`).
    NeedsString(&'static str),
    /// `#[rename_all]` names no rule.
    UnknownRule(String),
    /// `#[default(..)]` names no value it accepts.
    BadDefault(String),
}

/// The argument of `#[name("text")]`, or `None` when it is not one quoted
/// string.
fn quoted(tokens: Option<&str>) -> Option<String> {
    let text = tokens?.trim();
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.is_empty() && !inner.contains('"')).then(|| inner.to_string())
}

fn attr_name(attr: &crate::items::Attribute) -> Option<&str> {
    attr.path.segments.last().map(|s| s.name.name.as_str())
}

impl SerdeField {
    /// The options a field's attributes state, with any malformed ones.
    #[must_use]
    pub fn read(attrs: &Attrs) -> (Self, Vec<SerdeAttrProblem>) {
        let mut options = Self::default();
        let mut problems = Vec::new();
        for attr in &attrs.outer {
            match attr_name(attr) {
                Some("rename") => match quoted(attr.tokens.as_deref()) {
                    Some(key) => options.rename = Some(key),
                    None => problems.push(SerdeAttrProblem::NeedsString("rename")),
                },
                Some("skip") => options.skip = true,
                Some("default") => match attr.tokens.as_deref() {
                    None => options.default = Some(FieldDefault::Zero),
                    Some(text) => match DefaultValue::parse(text) {
                        Some(value) => options.default = Some(FieldDefault::Value(value)),
                        None => {
                            problems.push(SerdeAttrProblem::BadDefault(text.trim().to_string()));
                        }
                    },
                },
                _ => {}
            }
        }
        (options, problems)
    }

    /// The options a field's attributes state; malformed ones are left out.
    #[must_use]
    pub fn of(attrs: &Attrs) -> Self {
        Self::read(attrs).0
    }

    /// The key the field is written and read under.
    #[must_use]
    pub fn key(&self, name: &str, rule: Option<RenameRule>) -> String {
        self.rename
            .clone()
            .unwrap_or_else(|| rule.map_or_else(|| name.to_string(), |rule| rule.apply(name)))
    }
}

impl SerdeContainer {
    /// The options a struct's or enum's attributes state, with any malformed
    /// ones.
    #[must_use]
    pub fn read(attrs: &Attrs) -> (Self, Vec<SerdeAttrProblem>) {
        let mut options = Self::default();
        let mut problems = Vec::new();
        for attr in &attrs.outer {
            match attr_name(attr) {
                Some("rename_all") => match quoted(attr.tokens.as_deref()) {
                    Some(rule) => match RenameRule::named(&rule) {
                        Some(rule) => options.rename_all = Some(rule),
                        None => problems.push(SerdeAttrProblem::UnknownRule(rule)),
                    },
                    None => problems.push(SerdeAttrProblem::NeedsString("rename_all")),
                },
                Some("tag") => match quoted(attr.tokens.as_deref()) {
                    Some(tag) => options.tag = Some(tag),
                    None => problems.push(SerdeAttrProblem::NeedsString("tag")),
                },
                _ => {}
            }
        }
        (options, problems)
    }

    /// The options a struct's or enum's attributes state; malformed ones are
    /// left out.
    #[must_use]
    pub fn of(attrs: &Attrs) -> Self {
        Self::read(attrs).0
    }
}

/// The name a variant is written under: its `#[rename]`, else the
/// enum's rule applied, else its own name.
#[must_use]
pub fn variant_key(attrs: &Attrs, name: &str, rule: Option<RenameRule>) -> String {
    SerdeField::of(attrs).key(name, rule)
}

#[cfg(test)]
mod tests {
    use super::{DefaultValue, RenameRule};

    #[test]
    fn rename_rules_respell_fields_and_variants() {
        assert_eq!(RenameRule::Camel.apply("wind_speed_kmh"), "windSpeedKmh");
        assert_eq!(RenameRule::Pascal.apply("wind_speed"), "WindSpeed");
        assert_eq!(RenameRule::Snake.apply("PartlyCloudy"), "partly_cloudy");
        assert_eq!(
            RenameRule::ScreamingSnake.apply("PartlyCloudy"),
            "PARTLY_CLOUDY"
        );
        assert_eq!(RenameRule::Kebab.apply("http_status2xx"), "http-status2xx");
        assert_eq!(RenameRule::Lower.apply("PartlyCloudy"), "partlycloudy");
        assert_eq!(RenameRule::Camel.apply("Metric"), "metric");
        assert_eq!(RenameRule::named("camelCase"), Some(RenameRule::Camel));
        assert_eq!(RenameRule::named("camel"), None);
    }

    #[test]
    fn default_values_read_literals_none_and_variants() {
        assert_eq!(
            DefaultValue::parse("3"),
            Some(DefaultValue::Int("3".into()))
        );
        assert_eq!(
            DefaultValue::parse("-1_000"),
            Some(DefaultValue::Int("-1000".into()))
        );
        assert_eq!(
            DefaultValue::parse("2.5"),
            Some(DefaultValue::Float("2.5".into()))
        );
        assert_eq!(
            DefaultValue::parse("\"a\\\"b\""),
            Some(DefaultValue::Str("a\"b".into()))
        );
        assert_eq!(DefaultValue::parse("'x'"), Some(DefaultValue::Char('x')));
        assert_eq!(
            DefaultValue::parse(" true "),
            Some(DefaultValue::Bool(true))
        );
        assert_eq!(DefaultValue::parse("None"), Some(DefaultValue::None));
        assert_eq!(
            DefaultValue::parse("Units::Metric"),
            Some(DefaultValue::Variant("Metric".into()))
        );
        assert_eq!(DefaultValue::parse("make()"), None);
        assert_eq!(DefaultValue::parse("1 + 2"), None);
    }
}
