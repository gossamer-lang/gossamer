//! The tape a well-formed document reads into, by one loop over its bytes.
//!
//! [`tape`] accepts exactly the documents the checking walk in the parent
//! module accepts and records the same nodes, but it keeps no diagnostic
//! state: it answers `None` for anything it does not read through, and the
//! caller then reads the document with the checking walk, which reports the
//! error where there is one. A document is read once on the common path and
//! a second time only when it is malformed.

use super::{Tape, TapeKind, TapeNode, scan};

/// The tape of `source`, or `None` when the walk stops short of its end:
/// a malformed document, nesting deeper than `max_depth`, or a number too
/// large for an `f64`. `capacity` slots are reserved up front, a bound the
/// walk never exceeds, so recording never moves the nodes.
pub(super) fn tape(source: &str, max_depth: usize, capacity: usize) -> Option<Tape> {
    let mut walk = Walk {
        text: source,
        bytes: source.as_bytes(),
        at: 0,
        tape: Tape {
            nodes: Vec::with_capacity(capacity),
            objects: Vec::new(),
            arrays: Vec::new(),
        },
    };
    walk.document(max_depth)?;
    Some(walk.tape)
}

struct Walk<'a> {
    text: &'a str,
    bytes: &'a [u8],
    at: usize,
    tape: Tape,
}

/// An object or array the walk is inside: its node's slot and how many
/// members or elements it has read.
struct Open {
    slot: usize,
    count: u32,
    object: bool,
}

/// What the byte at the cursor must begin.
enum Next {
    Value,
    Key,
    AfterValue,
}

impl Walk<'_> {
    fn document(&mut self, max_depth: usize) -> Option<()> {
        let mut open: Vec<Open> = Vec::new();
        let mut next = Next::Value;
        loop {
            self.skip_whitespace();
            match next {
                Next::Value => {
                    let byte = *self.bytes.get(self.at)?;
                    next = match byte {
                        b'{' | b'[' => {
                            if open.len() >= max_depth {
                                return None;
                            }
                            let object = byte == b'{';
                            let (kind, close) = if object {
                                (TapeKind::Object, b'}')
                            } else {
                                (TapeKind::Array, b']')
                            };
                            let slot = self.open(kind);
                            self.at += 1;
                            open.push(Open {
                                slot,
                                count: 0,
                                object,
                            });
                            self.skip_whitespace();
                            if self.bytes.get(self.at) == Some(&close) {
                                self.at += 1;
                                self.close(open.pop()?);
                                Next::AfterValue
                            } else if object {
                                Next::Key
                            } else {
                                Next::Value
                            }
                        }
                        b'"' => {
                            self.string()?;
                            Next::AfterValue
                        }
                        b't' => self.literal(b"true", TapeKind::True)?,
                        b'f' => self.literal(b"false", TapeKind::False)?,
                        b'n' => self.literal(b"null", TapeKind::Null)?,
                        b'-' | b'0'..=b'9' => {
                            self.number()?;
                            Next::AfterValue
                        }
                        _ => return None,
                    };
                }
                Next::Key => {
                    if self.bytes.get(self.at) != Some(&b'"') {
                        return None;
                    }
                    self.string()?;
                    self.skip_whitespace();
                    if self.bytes.get(self.at) != Some(&b':') {
                        return None;
                    }
                    self.at += 1;
                    next = Next::Value;
                }
                Next::AfterValue => {
                    let Some(top) = open.last_mut() else {
                        return (self.at == self.bytes.len()).then_some(());
                    };
                    top.count += 1;
                    let object = top.object;
                    match self.bytes.get(self.at) {
                        Some(b',') => {
                            self.at += 1;
                            next = if object { Next::Key } else { Next::Value };
                        }
                        Some(b'}') if object => {
                            self.at += 1;
                            self.close(open.pop()?);
                        }
                        Some(b']') if !object => {
                            self.at += 1;
                            self.close(open.pop()?);
                        }
                        _ => return None,
                    }
                }
            }
        }
    }

    fn skip_whitespace(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.bytes.get(self.at) {
            self.at += 1;
        }
    }

    /// Records a node of `kind` spanning `start..start + len`. Only a
    /// document whose offsets and lengths fit a node is walked.
    #[inline]
    fn scalar(&mut self, kind: TapeKind, start: usize, len: usize) {
        self.tape
            .nodes
            .push(TapeNode::new(kind, start as u32, len as u32));
    }

    /// Records the container of `kind` opening at the cursor, with a word the
    /// close fills in; answers its slot.
    fn open(&mut self, kind: TapeKind) -> usize {
        let slot = self.tape.nodes.len();
        if kind == TapeKind::Object {
            self.tape.objects.push(slot as u32);
        } else {
            self.tape.arrays.push(slot as u32);
        }
        self.scalar(kind, self.at, 0);
        self.tape.nodes.push(TapeNode::from_word(0));
        slot
    }

    /// Closes `container`, which ends just before the cursor.
    fn close(&mut self, container: Open) {
        let extent = self.tape.nodes.len() as u64;
        let node = self.tape.nodes[container.slot];
        self.tape.nodes[container.slot] =
            TapeNode::new(node.kind(), node.start, self.at as u32 - node.start);
        self.tape.nodes[container.slot + 1] =
            TapeNode::from_word(extent | (u64::from(container.count) << 32));
    }

    fn literal(&mut self, spelling: &[u8], kind: TapeKind) -> Option<Next> {
        if !self.bytes[self.at..].starts_with(spelling) {
            return None;
        }
        self.scalar(kind, self.at, spelling.len());
        self.at += spelling.len();
        Some(Next::AfterValue)
    }

    /// Reads the string whose opening quote is at the cursor.
    fn string(&mut self) -> Option<()> {
        let start = self.at;
        let mut at = start + 1;
        let mut escaped = false;
        loop {
            at = scan::find(
                self.bytes,
                at,
                |w| scan::equal(w, b'"') | scan::equal(w, b'\\') | scan::below(w, 0x20),
                |b| matches!(b, b'"' | b'\\') || b < 0x20,
            );
            match *self.bytes.get(at)? {
                b'"' => break,
                b'\\' => {
                    escaped = true;
                    at = self.escape(at + 1)?;
                }
                _ => return None,
            }
        }
        let kind = if escaped {
            TapeKind::StrEscaped
        } else {
            TapeKind::Str
        };
        self.at = at + 1;
        self.scalar(kind, start, self.at - start);
        Some(())
    }

    /// Checks the escape whose letter is at `at`, answering the offset past
    /// it. A `\u` high surrogate must be followed by a `\u` low one, and a low
    /// one must not stand alone.
    fn escape(&self, at: usize) -> Option<usize> {
        match *self.bytes.get(at)? {
            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => Some(at + 1),
            b'u' => {
                let unit = self.hex4(at + 1)?;
                match unit {
                    0xD800..=0xDBFF => {
                        if self.bytes.get(at + 5..at + 7) != Some(b"\\u") {
                            return None;
                        }
                        let low = self.hex4(at + 7)?;
                        (0xDC00..=0xDFFF).contains(&low).then_some(at + 11)
                    }
                    0xDC00..=0xDFFF => None,
                    _ => Some(at + 5),
                }
            }
            _ => None,
        }
    }

    fn hex4(&self, at: usize) -> Option<u32> {
        self.bytes
            .get(at..at + 4)?
            .iter()
            .try_fold(0u32, |acc, &b| {
                let digit = char::from(b).to_digit(16)?;
                Some((acc << 4) | digit)
            })
    }

    /// Reads the number at the cursor, recording its value as the checking
    /// walk does: an integer that fits an `i64` as `Int`, one that fits a
    /// `u64` as `Uint`, and any other number, `-0` included, as `Float`.
    fn number(&mut self) -> Option<()> {
        let start = self.at;
        let negative = self.bytes[start] == b'-';
        let mut at = start + usize::from(negative);
        let mut digits = Digits::default();
        match self.bytes.get(at) {
            Some(b'0') => at = digits.take(self.bytes, at, 1),
            Some(b'1'..=b'9') => at = digits.take(self.bytes, at, usize::MAX),
            _ => return None,
        }
        let whole = digits.count;
        let mut integral = true;
        if self.bytes.get(at) == Some(&b'.') {
            integral = false;
            at = digits.take(self.bytes, at + 1, usize::MAX);
            if digits.count == whole {
                return None;
            }
        }
        let fraction = digits.count - whole;
        let mut exponent = Some(0i64);
        if let Some(b'e' | b'E') = self.bytes.get(at) {
            integral = false;
            at += 1;
            let sign = match self.bytes.get(at) {
                Some(b'-') => -1,
                Some(b'+') => 1,
                _ => 0,
            };
            at += usize::from(sign != 0);
            let mut written = Digits::default();
            at = written.take(self.bytes, at, usize::MAX);
            if written.count == 0 {
                return None;
            }
            exponent = written
                .exact()
                .and_then(|n| i64::try_from(n).ok())
                .map(|n| if sign < 0 { -n } else { n });
        }
        self.at = at;
        let (kind, word) = match (integral, digits.exact()) {
            // Eighteen digits stay below 10^18, inside an `i64` either side
            // of zero.
            (true, Some(magnitude)) if digits.count <= 18 => {
                let magnitude = magnitude as i64;
                match (negative, magnitude) {
                    (true, 0) => (TapeKind::Float, (-0.0f64).to_bits()),
                    (true, n) => (TapeKind::Int, (-n) as u64),
                    (false, n) => (TapeKind::Int, n as u64),
                }
            }
            (false, Some(mantissa)) => {
                let scale = exponent.and_then(|e| e.checked_sub(i64::try_from(fraction).ok()?));
                match exact_float(mantissa, scale) {
                    Some(value) => {
                        let value = if negative { -value } else { value };
                        (TapeKind::Float, value.to_bits())
                    }
                    None => self.parsed(start, at, integral)?,
                }
            }
            _ => self.parsed(start, at, integral)?,
        };
        self.scalar(kind, start, at - start);
        self.tape.nodes.push(TapeNode::from_word(word));
        Some(())
    }

    /// The number `start..at` read from its text, for a value the digits
    /// alone do not settle.
    fn parsed(&self, start: usize, at: usize, integral: bool) -> Option<(TapeKind, u64)> {
        let text = &self.text[start..at];
        if let (true, Ok(n)) = (integral, text.parse::<i64>()) {
            return Some((TapeKind::Int, n as u64));
        }
        if let (true, Ok(n)) = (integral, text.parse::<u64>()) {
            return Some((TapeKind::Uint, n));
        }
        let value = text.parse::<f64>().ok().filter(|v| v.is_finite())?;
        Some((TapeKind::Float, value.to_bits()))
    }
}

/// The decimal digits of a number read so far, as one integer while it fits.
#[derive(Default)]
struct Digits {
    value: u64,
    count: usize,
}

impl Digits {
    /// Reads up to `limit` digits from `at`, answering the offset past them.
    fn take(&mut self, bytes: &[u8], mut at: usize, limit: usize) -> usize {
        let mut taken = 0;
        while limit - taken >= 8
            && let Some(chunk) = bytes.get(at..).and_then(<[u8]>::first_chunk::<8>)
        {
            let word = u64::from_le_bytes(*chunk);
            if !eight_digits(word) {
                break;
            }
            self.value = self
                .value
                .wrapping_mul(100_000_000)
                .wrapping_add(eight_digit_value(word));
            self.count += 8;
            taken += 8;
            at += 8;
        }
        while taken < limit
            && let Some(&d @ b'0'..=b'9') = bytes.get(at)
        {
            self.value = self
                .value
                .wrapping_mul(10)
                .wrapping_add(u64::from(d - b'0'));
            self.count += 1;
            taken += 1;
            at += 1;
        }
        at
    }

    /// The digits as an integer, when there are few enough that it did not
    /// wrap: nineteen digits stay below 10^19, inside a `u64`.
    fn exact(&self) -> Option<u64> {
        (self.count <= 19).then_some(self.value)
    }
}

/// Whether all eight bytes of `word` are ASCII digits: each byte's high
/// nibble is 3, and adding 6 to its low nibble does not carry out of it.
fn eight_digits(word: u64) -> bool {
    const HIGH: u64 = 0xF0F0_F0F0_F0F0_F0F0;
    const THREES: u64 = 0x3030_3030_3030_3030;
    word & HIGH == THREES && word.wrapping_add(0x0606_0606_0606_0606) & HIGH == THREES
}

/// The value of the eight ASCII digits in `word`, the first in its low byte,
/// combined pairwise: adjacent digits, then pairs, then quads.
fn eight_digit_value(word: u64) -> u64 {
    const MASK: u64 = 0x0000_00FF_0000_00FF;
    const HUNDREDS: u64 = 100 + (1_000_000 << 32);
    const UNITS: u64 = 1 + (10_000 << 32);
    let digits = word - 0x3030_3030_3030_3030;
    let pairs = digits.wrapping_mul(10).wrapping_add(digits >> 8);
    let high = (pairs & MASK).wrapping_mul(HUNDREDS);
    let low = ((pairs >> 16) & MASK).wrapping_mul(UNITS);
    u64::from((high.wrapping_add(low) >> 32) as u32)
}

/// `mantissa` × 10^`scale` when both factors are exact in an `f64`: the
/// mantissa below 2^53 and the power of ten at most 10^22. The product or
/// quotient of two exact values is then rounded once, to the same nearest
/// `f64` the text parses to.
fn exact_float(mantissa: u64, scale: Option<i64>) -> Option<f64> {
    const POWERS: [f64; 23] = [
        1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
        1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
    ];
    if mantissa > 1 << 53 {
        return None;
    }
    let scale = scale?;
    let power = *POWERS.get(usize::try_from(scale.unsigned_abs()).ok()?)?;
    #[allow(
        clippy::cast_precision_loss,
        reason = "a mantissa at most 2^53 converts exactly"
    )]
    let mantissa = mantissa as f64;
    Some(if scale >= 0 {
        mantissa * power
    } else {
        mantissa / power
    })
}

#[cfg(test)]
mod tests {
    use super::super::{checked_tape, max_depth, tape_bound};
    use super::tape;

    fn walked(text: &str) -> Option<super::Tape> {
        tape(text, max_depth(), tape_bound(text.as_bytes()))
    }

    #[test]
    fn the_walk_records_the_tape_the_checking_walk_records() {
        let documents = [
            r#"{"a": [1, -2, 3.5, -0, 1e3, true, false, null], "b": {"c": "d\n\u00e9"}}"#,
            "[18446744073709551615, -9223372036854775808, 9223372036854775807, 123456789012345678901234]",
            r#"{"k": "\ud83d\ude00", "": [], "e": {}}"#,
            "  \"text\"  ",
            "0",
            r#"[[[]], {"x": [{"y": 1}]}]"#,
            "[0.1, 0.12345678, -0.0, 0.0, 1e22, 1e23, 9007199254740993.0, 1.5e-300, 2.5E+3, 123.456e-2]",
            "[0.30000000000000004, 1234567890123456789.5, 0.00000000000000000000000001, 4.9e-324]",
        ];
        for text in documents {
            let expected = checked_tape(text, tape_bound(text.as_bytes())).unwrap();
            assert_eq!(walked(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn numbers_of_every_shape_read_as_the_checking_walk_reads_them() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        let mut text = String::from("[");
        for i in 0..4000 {
            if i > 0 {
                text.push(',');
            }
            if next(2) == 0 {
                text.push('-');
            }
            let magnitude = 10u64.pow(u32::try_from(next(12)).unwrap_or(0));
            let whole = next(magnitude);
            text.push_str(&whole.to_string());
            if next(4) != 0 {
                let width = usize::try_from(1 + next(20)).unwrap_or(1);
                let fraction = next(u64::MAX);
                text.push_str(&format!(".{fraction:0>width$}")[..=width]);
            }
            if next(3) == 0 {
                let exponent = i64::try_from(next(60)).unwrap_or(0) - 30;
                text.push_str(&format!("e{exponent}"));
            }
        }
        text.push(']');
        let expected = checked_tape(&text, tape_bound(text.as_bytes())).unwrap();
        assert_eq!(walked(&text), Some(expected));
    }

    #[test]
    fn eight_digits_read_as_their_decimal_value() {
        for text in ["00000000", "12345678", "99999999", "10000001", "87654321"] {
            let word = u64::from_le_bytes(*text.as_bytes().first_chunk::<8>().unwrap());
            assert!(super::eight_digits(word), "{text}");
            assert_eq!(super::eight_digit_value(word), text.parse::<u64>().unwrap());
        }
        for text in [
            "1234567a",
            "/2345678",
            ":2345678",
            "1234 678",
            "\u{0}2345678",
        ] {
            let mut bytes = [b'0'; 8];
            bytes.copy_from_slice(&text.as_bytes()[..8]);
            assert!(!super::eight_digits(u64::from_le_bytes(bytes)), "{text}");
        }
    }

    #[test]
    fn the_walk_stops_on_every_malformed_document() {
        let documents = [
            "",
            "[",
            "[1,]",
            "{\"a\" 1}",
            "{\"a\": 1,}",
            "01",
            "1.",
            "1e",
            "-",
            "tru",
            "nul",
            "\"\\x\"",
            "\"\\ud800\"",
            "\"\\udc00\"",
            "\"a\nb\"",
            "[1] 2",
            "{1: 2}",
            "1e999",
            "[1}",
            "{\"a\": 1]",
            "\"\\u12g4\"",
        ];
        for text in documents {
            assert_eq!(walked(text), None, "{text:?}");
        }
    }
}
