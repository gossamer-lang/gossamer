//! The JSON text a composite value crosses the binding boundary as.
//!
//! A value whose shape nests composites (`Option<(i64, String)>`,
//! `Vec<Bytes>`, `Result<Vec<Probe>, String>`) crosses every tier as JSON
//! text, written and read by the walk the type checker's serde pass generates
//! on the Gossamer side and by [`WireJson`] here. The two spell each shape
//! alike:
//!
//! | Shape | Text |
//! |---|---|
//! | integers, floats, `bool` | the number, `true` / `false` |
//! | `char`, `String` | a string |
//! | `Bytes`, `Vec<T>`, tuples | an array |
//! | `Option<T>` | `[]` for `None`, `[v]` for `Some(v)` |
//! | `Result<T, E>` | `[0, v]` for `Ok(v)`, `[1, e]` for `Err(e)` |
//! | `HashMap<String, V>`, a `GosStruct` | an object |
//!
//! `Option` and `Result` are arrays so `Some(None)` and `None` stay apart.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::BuildHasher;

use crate::conv::Bytes;
use crate::error::GosError;

/// A type that crosses inside a composite as JSON text.
pub trait WireJson: Sized {
    /// Appends the value's text to `out`.
    fn write_json(&self, out: &mut String);
    /// Reads one value at the parser's position.
    ///
    /// # Errors
    /// The text there is not this type's spelling.
    fn read_json(p: &mut Parser<'_>) -> Result<Self, String>;
}

/// A composite whose container crosses as JSON text: a binding item taking
/// or answering an `Option`, `Vec`, `Result`, or `HashMap<String, _>` of one
/// crosses whole as text.
pub trait Nested: WireJson {}

/// `value` as its JSON text.
#[must_use]
pub fn to_text<T: WireJson>(value: &T) -> String {
    let mut out = String::new();
    value.write_json(&mut out);
    out
}

/// The value `text` spells.
///
/// # Errors
/// `text` is not one value of `T`'s spelling.
pub fn from_text<T: WireJson>(text: &str) -> Result<T, String> {
    let mut p = Parser::new(text);
    let value = T::read_json(&mut p)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(format!("trailing text at byte {}", p.pos));
    }
    Ok(value)
}

/// A cursor over JSON text.
#[derive(Debug)]
pub struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    /// A parser at the start of `text`.
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.bytes.get(self.pos) {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected `{}` at byte {}", byte as char, self.pos))
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), String> {
        self.skip_ws();
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(())
        } else {
            Err(format!("expected `{word}` at byte {}", self.pos))
        }
    }

    /// Reads `[` v, .. `]`, calling `each` at every element.
    ///
    /// # Errors
    /// The text there is not an array, or `each` fails.
    pub fn array(
        &mut self,
        mut each: impl FnMut(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        self.expect(b'[')?;
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(());
        }
        loop {
            each(self)?;
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(());
                }
                _ => return Err(format!("expected `,` or `]` at byte {}", self.pos)),
            }
        }
    }

    /// Reads `{` "key": v, .. `}`, calling `each` with every key at its value.
    ///
    /// # Errors
    /// The text there is not an object, or `each` fails.
    pub fn object(
        &mut self,
        mut each: impl FnMut(&mut Self, String) -> Result<(), String>,
    ) -> Result<(), String> {
        self.expect(b'{')?;
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(());
        }
        loop {
            let key = self.string()?;
            self.expect(b':')?;
            each(self, key)?;
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(());
                }
                _ => return Err(format!("expected `,` or `}}` at byte {}", self.pos)),
            }
        }
    }

    /// Reads the element at `index` of a fixed-length array, after `[` or
    /// the previous element.
    ///
    /// # Errors
    /// The array ends early.
    pub fn element(&mut self, index: usize) -> Result<(), String> {
        if index == 0 {
            self.expect(b'[')
        } else {
            self.expect(b',')
        }
    }

    /// Reads the `]` that closes a fixed-length array.
    ///
    /// # Errors
    /// The array holds more elements.
    pub fn end_array(&mut self) -> Result<(), String> {
        self.expect(b']')
    }

    /// Skips one value of any shape.
    ///
    /// # Errors
    /// The text there is not a value.
    pub fn skip_value(&mut self) -> Result<(), String> {
        match self.peek() {
            Some(b'[') => self.array(Self::skip_value),
            Some(b'{') => self.object(|p, _| p.skip_value()),
            Some(b'"') => self.string().map(drop),
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            _ => self.number().map(drop),
        }
    }

    fn number(&mut self) -> Result<&'a str, String> {
        self.skip_ws();
        let start = self.pos;
        while let Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') = self.bytes.get(self.pos) {
            self.pos += 1;
        }
        if start == self.pos {
            return Err(format!("expected a number at byte {start}"));
        }
        std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|e| e.to_string())
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            let Some(&byte) = self.bytes.get(self.pos) else {
                return Err("unterminated string".to_string());
            };
            self.pos += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(&escape) = self.bytes.get(self.pos) else {
                        return Err("unterminated escape".to_string());
                    };
                    self.pos += 1;
                    match escape {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'u' => {
                            let c = self.unicode_escape()?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        }
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
        String::from_utf8(out).map_err(|e| e.to_string())
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .bytes
            .get(self.pos..self.pos + 4)
            .and_then(|d| std::str::from_utf8(d).ok())
            .ok_or("short `\\u` escape")?;
        self.pos += 4;
        u32::from_str_radix(digits, 16).map_err(|e| e.to_string())
    }

    fn unicode_escape(&mut self) -> Result<char, String> {
        let high = self.hex4()?;
        if (0xD800..0xDC00).contains(&high) {
            if !self.bytes[self.pos..].starts_with(b"\\u") {
                return Err("unpaired surrogate".to_string());
            }
            self.pos += 2;
            let low = self.hex4()?;
            let code = 0x10000 + ((high - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
            return char::from_u32(code).ok_or_else(|| "invalid surrogate pair".to_string());
        }
        char::from_u32(high).ok_or_else(|| format!("invalid code point {high:#x}"))
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

macro_rules! wire_int {
    ($($t:ty),*) => {$(
        impl WireJson for $t {
            fn write_json(&self, out: &mut String) {
                let _ = write!(out, "{self}");
            }
            fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
                let text = p.number()?;
                text.parse().map_err(|_| format!("expected an integer, found `{text}`"))
            }
        }
    )*};
}

wire_int!(i64, u64, usize, u8, u16, i32);

impl WireJson for f64 {
    fn write_json(&self, out: &mut String) {
        if self.is_finite() {
            let _ = write!(out, "{self}");
        } else {
            out.push_str("null");
        }
    }
    fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
        let text = p.number()?;
        text.parse()
            .map_err(|_| format!("expected a number, found `{text}`"))
    }
}

impl WireJson for bool {
    fn write_json(&self, out: &mut String) {
        out.push_str(if *self { "true" } else { "false" });
    }
    fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
        if p.peek() == Some(b't') {
            p.literal("true").map(|()| true)
        } else {
            p.literal("false").map(|()| false)
        }
    }
}

impl WireJson for char {
    fn write_json(&self, out: &mut String) {
        let mut buf = [0u8; 4];
        write_string(self.encode_utf8(&mut buf), out);
    }
    fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
        let text = p.string()?;
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Ok(c),
            _ => Err(format!("expected one character, found {text:?}")),
        }
    }
}

impl WireJson for String {
    fn write_json(&self, out: &mut String) {
        write_string(self, out);
    }
    fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
        p.string()
    }
}

impl WireJson for () {
    fn write_json(&self, out: &mut String) {
        out.push_str("null");
    }
    fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
        p.literal("null")
    }
}

impl WireJson for GosError {
    fn write_json(&self, out: &mut String) {
        write_string(&crate::native::WireError::into_wire(self.clone()), out);
    }
    fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
        p.string().map(GosError::new)
    }
}

impl WireJson for Bytes {
    fn write_json(&self, out: &mut String) {
        self.0.write_json(out);
    }
    fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
        Vec::<u8>::read_json(p).map(Bytes)
    }
}

impl Nested for Bytes {}

impl<T: WireJson> WireJson for Vec<T> {
    fn write_json(&self, out: &mut String) {
        out.push('[');
        for (i, item) in self.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            item.write_json(out);
        }
        out.push(']');
    }
    fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
        let mut items = Vec::new();
        p.array(|p| {
            items.push(T::read_json(p)?);
            Ok(())
        })?;
        Ok(items)
    }
}

impl<T: WireJson> Nested for Vec<T> {}

impl<T: WireJson> WireJson for Option<T> {
    fn write_json(&self, out: &mut String) {
        out.push('[');
        if let Some(value) = self {
            value.write_json(out);
        }
        out.push(']');
    }
    fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
        let mut value = None;
        p.array(|p| {
            if value.is_some() {
                return Err("an option holds one value".to_string());
            }
            value = Some(T::read_json(p)?);
            Ok(())
        })?;
        Ok(value)
    }
}

impl<T: WireJson> Nested for Option<T> {}

impl<T: WireJson, E: WireJson> WireJson for Result<T, E> {
    fn write_json(&self, out: &mut String) {
        match self {
            Ok(value) => {
                out.push_str("[0,");
                value.write_json(out);
            }
            Err(error) => {
                out.push_str("[1,");
                error.write_json(out);
            }
        }
        out.push(']');
    }
    fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
        p.element(0)?;
        let tag = i64::read_json(p)?;
        p.element(1)?;
        let value = match tag {
            0 => Ok(T::read_json(p)?),
            1 => Err(E::read_json(p)?),
            other => return Err(format!("expected a result tag, found {other}")),
        };
        p.end_array()?;
        Ok(value)
    }
}

impl<T: WireJson, E: WireJson> Nested for Result<T, E> {}

impl<V: WireJson, S: BuildHasher + Default> WireJson for HashMap<String, V, S> {
    fn write_json(&self, out: &mut String) {
        let mut keys: Vec<&String> = self.keys().collect();
        keys.sort();
        out.push('{');
        for (i, key) in keys.into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_string(key, out);
            out.push(':');
            self[key].write_json(out);
        }
        out.push('}');
    }
    fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
        let mut map = HashMap::default();
        p.object(|p, key| {
            map.insert(key, V::read_json(p)?);
            Ok(())
        })?;
        Ok(map)
    }
}

impl<V: WireJson, S: BuildHasher + Default> Nested for HashMap<String, V, S> {}

macro_rules! wire_tuple {
    ($( ($($t:ident $i:tt),+) )*) => {$(
        impl<$($t: WireJson),+> WireJson for ($($t,)+) {
            fn write_json(&self, out: &mut String) {
                out.push('[');
                $(
                    if $i > 0 {
                        out.push(',');
                    }
                    self.$i.write_json(out);
                )+
                out.push(']');
            }
            fn read_json(p: &mut Parser<'_>) -> Result<Self, String> {
                let value = ($({
                    p.element($i)?;
                    $t::read_json(p)?
                },)+);
                p.end_array()?;
                Ok(value)
            }
        }

        impl<$($t: WireJson),+> Nested for ($($t,)+) {}
    )*};
}

wire_tuple! {
    (A 0, B 1)
    (A 0, B 1, C 2)
    (A 0, B 1, C 2, D 3)
}

/// Reads the object a `GosStruct` crosses as, calling `field` with every key
/// at its value; a key the struct does not declare is skipped.
///
/// # Errors
/// The text is not an object, or `field` fails.
pub fn read_struct(
    p: &mut Parser<'_>,
    mut field: impl FnMut(&mut Parser<'_>, &str) -> Result<bool, String>,
) -> Result<(), String> {
    p.object(|p, key| {
        if field(p, &key)? {
            Ok(())
        } else {
            p.skip_value()
        }
    })
}

/// Writes the key of a `GosStruct` field, `first` for the first field.
pub fn write_field_key(key: &str, first: bool, out: &mut String) {
    if !first {
        out.push(',');
    }
    write_string(key, out);
    out.push(':');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: WireJson + PartialEq + std::fmt::Debug>(value: T, text: &str) {
        assert_eq!(to_text(&value), text);
        assert_eq!(from_text::<T>(text).unwrap(), value);
    }

    #[test]
    fn nested_shapes_round_trip_in_their_spelling() {
        round_trip(Some((3_i64, "m".to_string())), "[[3,\"m\"]]");
        round_trip(None::<(i64, String)>, "[]");
        round_trip(Some(None::<i64>), "[[]]");
        round_trip(Ok::<Option<i64>, String>(Some(7)), "[0,[7]]");
        round_trip(
            Err::<(i64, i64), String>("no dash".into()),
            "[1,\"no dash\"]",
        );
        round_trip(vec![Bytes(vec![97, 98])], "[[97,98]]");
        round_trip(vec![1.5_f64, -2.0], "[1.5,-2]");
        round_trip(
            HashMap::from([("k".to_string(), vec!['x'])]),
            "{\"k\":[\"x\"]}",
        );
        round_trip("q\"\\\n\u{1}".to_string(), "\"q\\\"\\\\\\n\\u0001\"");
    }

    #[test]
    fn escapes_the_runtime_writes_read_back() {
        assert_eq!(
            from_text::<String>("\"\\u003c\\ud83d\\ude00\"").unwrap(),
            "<\u{1F600}"
        );
        assert_eq!(from_text::<f64>("2").unwrap(), 2.0);
        assert!(from_text::<i64>("[1]").is_err());
        assert!(from_text::<Vec<i64>>("[1] x").is_err());
    }
}
