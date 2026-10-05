//! The method surface of `char` and the integer types, written in Gossamer.
//!
//! Each method is ordinary Gossamer source over operations every tier already
//! implements (`std::unicode`, wrapping arithmetic, comparisons), so it
//! compiles to the same code on the bytecode VM, the JIT, and a native build
//! from one definition. A method is injected only when the program names it,
//! as an `impl` block for each type that declares it, and never when the
//! program's own `impl` for that type already defines the name.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use gossamer_lex::{Keyword, Lexer, Punct, SourceMap, TokenKind};

/// The types a method is declared for.
#[derive(Clone, Copy)]
enum Receivers {
    Char,
    /// `u8`, whose ASCII tests mirror `char`'s.
    Byte,
    Signed,
    Unsigned,
    Integers,
    Floats,
    /// `Vec<T>`, declared once for every element type.
    Vector,
}

impl Receivers {
    fn types(self) -> &'static [&'static str] {
        const SIGNED: &[&str] = &["i8", "i16", "i32", "i64", "isize"];
        const UNSIGNED: &[&str] = &["u8", "u16", "u32", "u64", "usize"];
        const INTEGERS: &[&str] = &[
            "i8", "i16", "i32", "i64", "isize", "u8", "u16", "u32", "u64", "usize",
        ];
        match self {
            Self::Char => &["char"],
            Self::Byte => &["u8"],
            Self::Signed => SIGNED,
            Self::Unsigned => UNSIGNED,
            Self::Integers => INTEGERS,
            Self::Floats => &["f64", "f32"],
            Self::Vector => &["Vec"],
        }
    }
}

/// One method: the types it is declared for, the methods of the same type
/// its body calls, and its source, where `T` names the receiver type and
/// `U` the unsigned type of the same width.
struct Method {
    name: &'static str,
    receivers: Receivers,
    needs: &'static [&'static str],
    source: &'static str,
}

const fn method(
    name: &'static str,
    receivers: Receivers,
    needs: &'static [&'static str],
    source: &'static str,
) -> Method {
    Method {
        name,
        receivers,
        needs,
        source,
    }
}

/// The helper module the `char` methods reach `std::unicode` through: an
/// injected item cannot add a `use` to the program's own list.
const CHAR_HELPERS: &str = r"
mod __gos_prim_char {
    use std::unicode
    pub fn is_letter(c: char) -> bool { unicode::is_letter(c) }
    pub fn is_number(c: char) -> bool { unicode::is_number(c) }
    pub fn is_space(c: char) -> bool { unicode::is_space(c) }
    pub fn is_upper(c: char) -> bool { unicode::is_upper(c) }
    pub fn is_lower(c: char) -> bool { unicode::is_lower(c) }
    pub fn is_control(c: char) -> bool { unicode::is_control(c) }
    pub fn upper(c: char) -> String { unicode::to_upper_str(c.to_string()) }
    pub fn lower(c: char) -> String { unicode::to_lower_str(c.to_string()) }
}
";

const METHODS: &[Method] = &[
    // `char`: Unicode classification and conversion.
    method(
        "is_alphabetic",
        Receivers::Char,
        &[],
        "fn is_alphabetic(&self) -> bool { __gos_prim_char::is_letter(*self) }",
    ),
    method(
        "is_numeric",
        Receivers::Char,
        &[],
        "fn is_numeric(&self) -> bool { __gos_prim_char::is_number(*self) }",
    ),
    method(
        "is_alphanumeric",
        Receivers::Char,
        &[],
        "fn is_alphanumeric(&self) -> bool { __gos_prim_char::is_letter(*self) || __gos_prim_char::is_number(*self) }",
    ),
    method(
        "is_whitespace",
        Receivers::Char,
        &[],
        "fn is_whitespace(&self) -> bool { __gos_prim_char::is_space(*self) }",
    ),
    method(
        "is_uppercase",
        Receivers::Char,
        &[],
        "fn is_uppercase(&self) -> bool { __gos_prim_char::is_upper(*self) }",
    ),
    method(
        "is_lowercase",
        Receivers::Char,
        &[],
        "fn is_lowercase(&self) -> bool { __gos_prim_char::is_lower(*self) }",
    ),
    method(
        "is_control",
        Receivers::Char,
        &[],
        "fn is_control(&self) -> bool { __gos_prim_char::is_control(*self) }",
    ),
    method(
        "to_uppercase",
        Receivers::Char,
        &[],
        "fn to_uppercase(&self) -> String { __gos_prim_char::upper(*self) }",
    ),
    method(
        "to_lowercase",
        Receivers::Char,
        &[],
        "fn to_lowercase(&self) -> String { __gos_prim_char::lower(*self) }",
    ),
    method(
        "is_ascii",
        Receivers::Char,
        &[],
        "fn is_ascii(&self) -> bool { (*self as i64) < 128 }",
    ),
    method(
        "to_digit",
        Receivers::Char,
        &[],
        r"fn to_digit(&self, radix: i64) -> Option<i64> {
        let c = *self
        let d = if c >= '0' && c <= '9' {
            c as i64 - '0' as i64
        } else if c >= 'a' && c <= 'z' {
            c as i64 - 'a' as i64 + 10
        } else if c >= 'A' && c <= 'Z' {
            c as i64 - 'A' as i64 + 10
        } else {
            return None
        }
        if d < radix { Some(d) } else { None }
    }",
    ),
    method(
        "is_digit",
        Receivers::Char,
        &["to_digit"],
        "fn is_digit(&self, radix: i64) -> bool { self.to_digit(radix).is_some() }",
    ),
    method(
        "len_utf8",
        Receivers::Char,
        &[],
        r"fn len_utf8(&self) -> i64 {
        let n = *self as i64
        if n < 0x80 { 1 } else if n < 0x800 { 2 } else if n < 0x10000 { 3 } else { 4 }
    }",
    ),
    method(
        "len_utf16",
        Receivers::Char,
        &[],
        "fn len_utf16(&self) -> i64 { if (*self as i64) < 0x10000 { 1 } else { 2 } }",
    ),
    method(
        "from_digit",
        Receivers::Char,
        &[],
        r"fn from_digit(digit: i64, radix: i64) -> Option<char> {
        if radix < 2 || radix > 36 || digit < 0 || digit >= radix { return None }
        if digit < 10 {
            Some(('0' as u8 + digit as u8) as char)
        } else {
            Some(('a' as u8 + (digit - 10) as u8) as char)
        }
    }",
    ),
    method(
        "from_u32",
        Receivers::Char,
        &[],
        r"fn from_u32(code: i64) -> Option<char> {
        if code < 0 || code > 0x10FFFF || (code >= 0xD800 && code <= 0xDFFF) { return None }
        let mut bytes: Vec<u8> = #[]
        if code < 0x80 {
            bytes.push(code as u8)
        } else if code < 0x800 {
            bytes.push((0xC0 | (code >> 6)) as u8)
            bytes.push((0x80 | (code & 0x3F)) as u8)
        } else if code < 0x10000 {
            bytes.push((0xE0 | (code >> 12)) as u8)
            bytes.push((0x80 | ((code >> 6) & 0x3F)) as u8)
            bytes.push((0x80 | (code & 0x3F)) as u8)
        } else {
            bytes.push((0xF0 | (code >> 18)) as u8)
            bytes.push((0x80 | ((code >> 12) & 0x3F)) as u8)
            bytes.push((0x80 | ((code >> 6) & 0x3F)) as u8)
            bytes.push((0x80 | (code & 0x3F)) as u8)
        }
        let mut text = String::new()
        if text.push_utf8(bytes, 0, bytes.len()) { Some(text[0]) } else { None }
    }",
    ),
    // ASCII tests on `char` and `u8`, where `T` is the receiver.
    method(
        "is_ascii_digit",
        Receivers::Char,
        &[],
        "fn is_ascii_digit(&self) -> bool { *self >= '0' && *self <= '9' }",
    ),
    method(
        "is_ascii_digit",
        Receivers::Byte,
        &[],
        "fn is_ascii_digit(&self) -> bool { *self >= b'0' && *self <= b'9' }",
    ),
    method(
        "is_ascii_uppercase",
        Receivers::Char,
        &[],
        "fn is_ascii_uppercase(&self) -> bool { *self >= 'A' && *self <= 'Z' }",
    ),
    method(
        "is_ascii_uppercase",
        Receivers::Byte,
        &[],
        "fn is_ascii_uppercase(&self) -> bool { *self >= b'A' && *self <= b'Z' }",
    ),
    method(
        "is_ascii_lowercase",
        Receivers::Char,
        &[],
        "fn is_ascii_lowercase(&self) -> bool { *self >= 'a' && *self <= 'z' }",
    ),
    method(
        "is_ascii_lowercase",
        Receivers::Byte,
        &[],
        "fn is_ascii_lowercase(&self) -> bool { *self >= b'a' && *self <= b'z' }",
    ),
    method(
        "is_ascii_alphabetic",
        Receivers::Char,
        &["is_ascii_uppercase", "is_ascii_lowercase"],
        "fn is_ascii_alphabetic(&self) -> bool { self.is_ascii_uppercase() || self.is_ascii_lowercase() }",
    ),
    method(
        "is_ascii_alphabetic",
        Receivers::Byte,
        &["is_ascii_uppercase", "is_ascii_lowercase"],
        "fn is_ascii_alphabetic(&self) -> bool { self.is_ascii_uppercase() || self.is_ascii_lowercase() }",
    ),
    method(
        "is_ascii_alphanumeric",
        Receivers::Char,
        &["is_ascii_alphabetic", "is_ascii_digit"],
        "fn is_ascii_alphanumeric(&self) -> bool { self.is_ascii_alphabetic() || self.is_ascii_digit() }",
    ),
    method(
        "is_ascii_alphanumeric",
        Receivers::Byte,
        &["is_ascii_alphabetic", "is_ascii_digit"],
        "fn is_ascii_alphanumeric(&self) -> bool { self.is_ascii_alphabetic() || self.is_ascii_digit() }",
    ),
    method(
        "is_ascii_hexdigit",
        Receivers::Char,
        &["is_ascii_digit"],
        "fn is_ascii_hexdigit(&self) -> bool { self.is_ascii_digit() || (*self >= 'a' && *self <= 'f') || (*self >= 'A' && *self <= 'F') }",
    ),
    method(
        "is_ascii_hexdigit",
        Receivers::Byte,
        &["is_ascii_digit"],
        "fn is_ascii_hexdigit(&self) -> bool { self.is_ascii_digit() || (*self >= b'a' && *self <= b'f') || (*self >= b'A' && *self <= b'F') }",
    ),
    method(
        "is_ascii_whitespace",
        Receivers::Char,
        &[],
        "fn is_ascii_whitespace(&self) -> bool { *self == ' ' || *self == '\\t' || *self == '\\n' || *self == '\\r' || *self == '\\u{0C}' }",
    ),
    method(
        "is_ascii_whitespace",
        Receivers::Byte,
        &[],
        "fn is_ascii_whitespace(&self) -> bool { *self == b' ' || *self == b'\\t' || *self == b'\\n' || *self == b'\\r' || *self == 12 }",
    ),
    method(
        "is_ascii_punctuation",
        Receivers::Char,
        &[],
        "fn is_ascii_punctuation(&self) -> bool { let n = *self as i64; (n >= 33 && n <= 47) || (n >= 58 && n <= 64) || (n >= 91 && n <= 96) || (n >= 123 && n <= 126) }",
    ),
    method(
        "is_ascii_punctuation",
        Receivers::Byte,
        &[],
        "fn is_ascii_punctuation(&self) -> bool { let n = *self as i64; (n >= 33 && n <= 47) || (n >= 58 && n <= 64) || (n >= 91 && n <= 96) || (n >= 123 && n <= 126) }",
    ),
    method(
        "is_ascii_graphic",
        Receivers::Char,
        &[],
        "fn is_ascii_graphic(&self) -> bool { let n = *self as i64; n >= 33 && n <= 126 }",
    ),
    method(
        "is_ascii_graphic",
        Receivers::Byte,
        &[],
        "fn is_ascii_graphic(&self) -> bool { *self >= 33 && *self <= 126 }",
    ),
    method(
        "is_ascii_control",
        Receivers::Char,
        &[],
        "fn is_ascii_control(&self) -> bool { let n = *self as i64; n < 32 || n == 127 }",
    ),
    method(
        "is_ascii_control",
        Receivers::Byte,
        &[],
        "fn is_ascii_control(&self) -> bool { *self < 32 || *self == 127 }",
    ),
    method(
        "to_ascii_uppercase",
        Receivers::Char,
        &["is_ascii_lowercase"],
        "fn to_ascii_uppercase(&self) -> char { if self.is_ascii_lowercase() { (*self as u8 - 32) as char } else { *self } }",
    ),
    method(
        "to_ascii_uppercase",
        Receivers::Byte,
        &["is_ascii_lowercase"],
        "fn to_ascii_uppercase(&self) -> u8 { if self.is_ascii_lowercase() { *self - 32 } else { *self } }",
    ),
    method(
        "to_ascii_lowercase",
        Receivers::Char,
        &["is_ascii_uppercase"],
        "fn to_ascii_lowercase(&self) -> char { if self.is_ascii_uppercase() { (*self as u8 + 32) as char } else { *self } }",
    ),
    method(
        "to_ascii_lowercase",
        Receivers::Byte,
        &["is_ascii_uppercase"],
        "fn to_ascii_lowercase(&self) -> u8 { if self.is_ascii_uppercase() { *self + 32 } else { *self } }",
    ),
    method(
        "eq_ignore_ascii_case",
        Receivers::Char,
        &["to_ascii_lowercase"],
        "fn eq_ignore_ascii_case(&self, other: char) -> bool { self.to_ascii_lowercase() == other.to_ascii_lowercase() }",
    ),
    method(
        "eq_ignore_ascii_case",
        Receivers::Byte,
        &["to_ascii_lowercase"],
        "fn eq_ignore_ascii_case(&self, other: u8) -> bool { self.to_ascii_lowercase() == other.to_ascii_lowercase() }",
    ),
    // Integers: checked, saturating, and overflowing arithmetic. Plain
    // arithmetic reports an overflow, so the checked forms detect it on the
    // wrapped result instead of computing it.
    method(
        "checked_add",
        Receivers::Signed,
        &[],
        r"fn checked_add(&self, rhs: T) -> Option<T> {
        let r = *self +% rhs
        if (rhs > 0 && r < *self) || (rhs < 0 && r > *self) { None } else { Some(r) }
    }",
    ),
    method(
        "checked_add",
        Receivers::Unsigned,
        &[],
        "fn checked_add(&self, rhs: T) -> Option<T> { let r = *self +% rhs; if r < *self { None } else { Some(r) } }",
    ),
    method(
        "checked_sub",
        Receivers::Signed,
        &[],
        r"fn checked_sub(&self, rhs: T) -> Option<T> {
        let r = *self -% rhs
        if (rhs > 0 && r > *self) || (rhs < 0 && r < *self) { None } else { Some(r) }
    }",
    ),
    method(
        "checked_sub",
        Receivers::Unsigned,
        &[],
        "fn checked_sub(&self, rhs: T) -> Option<T> { if rhs > *self { None } else { Some(*self - rhs) } }",
    ),
    method(
        "checked_mul",
        Receivers::Signed,
        &[],
        r"fn checked_mul(&self, rhs: T) -> Option<T> {
        if *self == 0 || rhs == 0 { return Some(0) }
        if (*self == -1 && rhs == T::MIN) || (rhs == -1 && *self == T::MIN) { return None }
        let r = *self *% rhs
        if r / rhs != *self { None } else { Some(r) }
    }",
    ),
    method(
        "checked_mul",
        Receivers::Unsigned,
        &[],
        r"fn checked_mul(&self, rhs: T) -> Option<T> {
        if *self == 0 { return Some(0) }
        let r = *self *% rhs
        if r / *self != rhs { None } else { Some(r) }
    }",
    ),
    method(
        "checked_div",
        Receivers::Signed,
        &[],
        "fn checked_div(&self, rhs: T) -> Option<T> { if rhs == 0 || (*self == T::MIN && rhs == -1) { None } else { Some(*self / rhs) } }",
    ),
    method(
        "checked_div",
        Receivers::Unsigned,
        &[],
        "fn checked_div(&self, rhs: T) -> Option<T> { if rhs == 0 { None } else { Some(*self / rhs) } }",
    ),
    method(
        "checked_rem",
        Receivers::Signed,
        &[],
        "fn checked_rem(&self, rhs: T) -> Option<T> { if rhs == 0 || (*self == T::MIN && rhs == -1) { None } else { Some(*self % rhs) } }",
    ),
    method(
        "checked_rem",
        Receivers::Unsigned,
        &[],
        "fn checked_rem(&self, rhs: T) -> Option<T> { if rhs == 0 { None } else { Some(*self % rhs) } }",
    ),
    method(
        "checked_neg",
        Receivers::Signed,
        &[],
        "fn checked_neg(&self) -> Option<T> { if *self == T::MIN { None } else { Some(0 - *self) } }",
    ),
    method(
        "checked_abs",
        Receivers::Signed,
        &[],
        "fn checked_abs(&self) -> Option<T> { if *self == T::MIN { None } else if *self < 0 { Some(0 - *self) } else { Some(*self) } }",
    ),
    method(
        "checked_pow",
        Receivers::Integers,
        &["checked_mul"],
        r"fn checked_pow(&self, exp: i64) -> Option<T> {
        if exp < 0 { return None }
        let mut base = *self
        let mut e = exp
        let mut acc: T = 1
        while e > 0 {
            if e % 2 == 1 {
                acc = acc.checked_mul(base)?
            }
            e = e / 2
            if e > 0 {
                base = base.checked_mul(base)?
            }
        }
        Some(acc)
    }",
    ),
    method(
        "saturating_add",
        Receivers::Signed,
        &["checked_add"],
        "fn saturating_add(&self, rhs: T) -> T { match self.checked_add(rhs) { Some(r) => r, None => if rhs > 0 { T::MAX } else { T::MIN } } }",
    ),
    method(
        "saturating_add",
        Receivers::Unsigned,
        &["checked_add"],
        "fn saturating_add(&self, rhs: T) -> T { self.checked_add(rhs).unwrap_or(T::MAX) }",
    ),
    method(
        "saturating_sub",
        Receivers::Signed,
        &["checked_sub"],
        "fn saturating_sub(&self, rhs: T) -> T { match self.checked_sub(rhs) { Some(r) => r, None => if rhs > 0 { T::MIN } else { T::MAX } } }",
    ),
    method(
        "saturating_sub",
        Receivers::Unsigned,
        &["checked_sub"],
        "fn saturating_sub(&self, rhs: T) -> T { self.checked_sub(rhs).unwrap_or(0) }",
    ),
    method(
        "saturating_mul",
        Receivers::Signed,
        &["checked_mul"],
        "fn saturating_mul(&self, rhs: T) -> T { match self.checked_mul(rhs) { Some(r) => r, None => if (*self < 0) != (rhs < 0) { T::MIN } else { T::MAX } } }",
    ),
    method(
        "saturating_mul",
        Receivers::Unsigned,
        &["checked_mul"],
        "fn saturating_mul(&self, rhs: T) -> T { self.checked_mul(rhs).unwrap_or(T::MAX) }",
    ),
    method(
        "saturating_pow",
        Receivers::Signed,
        &["checked_pow"],
        "fn saturating_pow(&self, exp: i64) -> T { match self.checked_pow(exp) { Some(r) => r, None => if *self < 0 && exp % 2 == 1 { T::MIN } else { T::MAX } } }",
    ),
    method(
        "saturating_pow",
        Receivers::Unsigned,
        &["checked_pow"],
        "fn saturating_pow(&self, exp: i64) -> T { self.checked_pow(exp).unwrap_or(T::MAX) }",
    ),
    method(
        "overflowing_add",
        Receivers::Integers,
        &["checked_add"],
        "fn overflowing_add(&self, rhs: T) -> (T, bool) { (*self +% rhs, self.checked_add(rhs).is_none()) }",
    ),
    method(
        "overflowing_sub",
        Receivers::Integers,
        &["checked_sub"],
        "fn overflowing_sub(&self, rhs: T) -> (T, bool) { (*self -% rhs, self.checked_sub(rhs).is_none()) }",
    ),
    method(
        "overflowing_mul",
        Receivers::Integers,
        &["checked_mul"],
        "fn overflowing_mul(&self, rhs: T) -> (T, bool) { (*self *% rhs, self.checked_mul(rhs).is_none()) }",
    ),
    // Floats: the power spellings a Rust reader expects beside `pow`.
    method(
        "powf",
        Receivers::Floats,
        &[],
        "fn powf(&self, exp: T) -> T { self.pow(exp) }",
    ),
    method(
        "powi",
        Receivers::Floats,
        &[],
        "fn powi(&self, exp: i64) -> T { self.pow(exp as T) }",
    ),
    // `Vec<T>`: in-place filtering.
    method(
        "retain",
        Receivers::Vector,
        &[],
        r"fn retain(&mut self, keep: Fn(T) -> bool) {
        let kept = self.filter(keep)
        *self = kept
    }",
    ),
    // Integers: the rest of the numeric surface.
    method(
        "pow",
        Receivers::Integers,
        &[],
        r#"fn pow(&self, exp: i64) -> T {
        if exp < 0 { panic(format("negative exponent {exp} in an integer power")) }
        let mut base = *self
        let mut e = exp
        let mut acc: T = 1
        while e > 0 {
            if e % 2 == 1 {
                acc = acc * base
            }
            e = e / 2
            if e > 0 {
                base = base * base
            }
        }
        acc
    }"#,
    ),
    method(
        "rem_euclid",
        Receivers::Signed,
        &[],
        r"fn rem_euclid(&self, rhs: T) -> T {
        let r = *self % rhs
        if r < 0 { if rhs < 0 { r - rhs } else { r + rhs } } else { r }
    }",
    ),
    method(
        "rem_euclid",
        Receivers::Unsigned,
        &[],
        "fn rem_euclid(&self, rhs: T) -> T { *self % rhs }",
    ),
    method(
        "div_euclid",
        Receivers::Signed,
        &[],
        r"fn div_euclid(&self, rhs: T) -> T {
        let q = *self / rhs
        if *self % rhs < 0 { if rhs > 0 { q - 1 } else { q + 1 } } else { q }
    }",
    ),
    method(
        "div_euclid",
        Receivers::Unsigned,
        &[],
        "fn div_euclid(&self, rhs: T) -> T { *self / rhs }",
    ),
    method(
        "signum",
        Receivers::Signed,
        &[],
        "fn signum(&self) -> T { if *self > 0 { 1 } else if *self < 0 { -1 } else { 0 } }",
    ),
    method(
        "is_positive",
        Receivers::Signed,
        &[],
        "fn is_positive(&self) -> bool { *self > 0 }",
    ),
    method(
        "is_negative",
        Receivers::Signed,
        &[],
        "fn is_negative(&self) -> bool { *self < 0 }",
    ),
    method(
        "abs_diff",
        Receivers::Signed,
        &[],
        "fn abs_diff(&self, other: T) -> U { if *self > other { (*self as U) -% (other as U) } else { (other as U) -% (*self as U) } }",
    ),
    method(
        "abs_diff",
        Receivers::Unsigned,
        &[],
        "fn abs_diff(&self, other: T) -> T { if *self > other { *self - other } else { other - *self } }",
    ),
    method(
        "is_power_of_two",
        Receivers::Unsigned,
        &[],
        "fn is_power_of_two(&self) -> bool { *self != 0 && (*self & (*self - 1)) == 0 }",
    ),
    method(
        "next_power_of_two",
        Receivers::Unsigned,
        &[],
        r"fn next_power_of_two(&self) -> T {
        let mut p: T = 1
        while p < *self { p = p * 2 }
        p
    }",
    ),
    method(
        "isqrt",
        Receivers::Integers,
        &[],
        r#"fn isqrt(&self) -> T {
        if *self < 0 { panic("integer square root of a negative number") }
        if *self < 2 { return *self }
        let mut lo: T = 1
        let mut hi: T = *self
        while lo < hi {
            let mid = lo + (hi - lo + 1) / 2
            if mid <= *self / mid { lo = mid } else { hi = mid - 1 }
        }
        lo
    }"#,
    ),
    method(
        "count_ones",
        Receivers::Integers,
        &[],
        r"fn count_ones(&self) -> i64 {
        let mut x = *self as U
        let mut n = 0
        while x != 0 {
            x = x & (x -% 1)
            n += 1
        }
        n
    }",
    ),
    method(
        "count_zeros",
        Receivers::Integers,
        &["count_ones"],
        "fn count_zeros(&self) -> i64 { T::BITS as i64 - self.count_ones() }",
    ),
    method(
        "leading_zeros",
        Receivers::Integers,
        &[],
        r"fn leading_zeros(&self) -> i64 {
        let mut x = *self as U
        let mut n = T::BITS as i64
        while x != 0 {
            x = x >> 1
            n -= 1
        }
        n
    }",
    ),
    method(
        "trailing_zeros",
        Receivers::Integers,
        &[],
        r"fn trailing_zeros(&self) -> i64 {
        let mut x = *self as U
        if x == 0 { return T::BITS as i64 }
        let mut n = 0
        while x & 1 == 0 {
            x = x >> 1
            n += 1
        }
        n
    }",
    ),
];

/// The renderers a `{:+}` and a `{:e}` placeholder call. A sign is added to a
/// number that does not already carry one; scientific notation is read off
/// the number's shortest round-trip digits, rounded half away from zero when
/// a precision is asked for.
const FORMAT_SIGN_SOURCE: &str = r#"
fn __gos_fmt_sign(text: String) -> String {
    if text.starts_with("-") || text == "NaN" { text } else { "+" + text }
}
"#;

const FORMAT_EXPONENT_SOURCE: &str = r#"
fn __gos_fmt_exp(text: String, precision: i64, upper: bool) -> String {
    if text == "NaN" || text == "inf" || text == "-inf" { return text }
    let negative = text.starts_with("-")
    let body = if negative { text.substring(1, text.byte_len()) } else { text }
    let int_part, frac_part = match body.split_once(".") {
        Some(parts) => parts,
        None => (body, ""),
    }
    let digits = int_part + frac_part
    let marker = if upper { "E" } else { "e" }
    let sign = if negative { "-" } else { "" }
    let mut first = -1
    for i in 0..digits.byte_len() {
        if digits.byte_at(i) != b'0' {
            first = i
            break
        }
    }
    if first < 0 {
        let zeros = if precision > 0 { "." + "0".repeat(precision) } else { "" }
        return sign + "0" + zeros + marker + "0"
    }
    let mut exp = int_part.byte_len() - 1 - first
    let mut sig: Vec<i64> = #[]
    for i in first..digits.byte_len() { sig.push(digits.byte_at(i) - b'0' as i64) }
    while sig.len() > 1 && sig[sig.len() - 1] == 0 { let _ = sig.pop() }
    if precision >= 0 {
        let keep = precision + 1
        if sig.len() > keep {
            let round_up = sig[keep] >= 5
            sig.truncate(keep)
            if round_up {
                let mut i = keep - 1
                loop {
                    sig[i] += 1
                    if sig[i] < 10 { break }
                    sig[i] = 0
                    if i == 0 {
                        sig.insert(0, 1).unwrap_or(())
                        let _ = sig.pop()
                        exp += 1
                        break
                    }
                    i -= 1
                }
            }
        }
        while sig.len() < keep { sig.push(0) }
    }
    let mut mantissa = sig[0].to_string()
    if sig.len() > 1 {
        mantissa += "."
        for i in 1..sig.len() { mantissa += sig[i].to_string() }
    }
    sign + mantissa + marker + exp.to_string()
}
"#;

/// The format renderers `source`'s templates call, or an empty string.
#[must_use]
pub(crate) fn synthesize_format_helpers(source: &str) -> String {
    if !source.contains(":+") && !source.contains("e}") && !source.contains("E}") {
        return String::new();
    }
    let mut map = SourceMap::new();
    let file = map.add_file("<format-helper-scan>", String::new());
    let mut lexer = Lexer::new(source, file);
    let mut sign = false;
    let mut exponent = false;
    loop {
        let token = lexer.next_token();
        match token.kind {
            TokenKind::Eof => break,
            TokenKind::FStringLit | TokenKind::FTripleStringLit => {
                let text = source
                    .get(token.span.start as usize..token.span.end as usize)
                    .unwrap_or("");
                let triple = token.kind == TokenKind::FTripleStringLit;
                let quotes = if triple { 3 } else { 1 };
                let body = text
                    .get(1 + quotes..text.len().saturating_sub(quotes))
                    .unwrap_or("");
                let (s, e) = crate::expressions::interpolated_template_needs(body, triple);
                sign |= s;
                exponent |= e;
            }
            TokenKind::StringLit | TokenKind::TripleStringLit | TokenKind::RawStringLit { .. } => {
                let text = source
                    .get(token.span.start as usize..token.span.end as usize)
                    .unwrap_or("")
                    .trim_start_matches('r')
                    .trim_matches('#')
                    .trim_matches('"');
                let (s, e) = crate::expressions::format_template_needs(text);
                sign |= s;
                exponent |= e;
            }
            _ => {}
        }
    }
    let mut out = String::new();
    if sign {
        out.push_str(FORMAT_SIGN_SOURCE);
    }
    if exponent {
        out.push_str(FORMAT_EXPONENT_SOURCE);
    }
    out
}

/// The unsigned type of `ty`'s width.
fn unsigned_of(ty: &str) -> &'static str {
    match ty {
        "i8" | "u8" => "u8",
        "i16" | "u16" => "u16",
        "i32" | "u32" => "u32",
        "isize" | "usize" => "usize",
        _ => "u64",
    }
}

/// Every method name the table declares, which a source has to mention for
/// any of it to be injected.
fn table_names() -> impl Iterator<Item = &'static str> {
    METHODS.iter().map(|m| m.name)
}

/// The names `source` spells in method (`.name(`) or associated
/// (`Type::name(`) position, and the names each primitive's own `impl`
/// blocks already define.
struct Mentions {
    called: HashSet<String>,
    defined: HashSet<(String, String)>,
}

fn scan(source: &str) -> Mentions {
    let mut tokens = Vec::new();
    code_tokens(source, &mut tokens);
    let mut called = HashSet::new();
    let mut defined = HashSet::new();
    let mut i = 0;
    while i < tokens.len() {
        let (kind, _) = &tokens[i];
        let is_path_head = matches!(kind, TokenKind::Punct(Punct::Dot | Punct::ColonColon));
        if is_path_head
            && let Some((TokenKind::Ident, name)) = tokens.get(i + 1)
            && matches!(
                tokens.get(i + 2),
                Some((TokenKind::Punct(Punct::LParen), _))
            )
        {
            called.insert(name.clone());
        }
        // `impl char { .. fn name .. }` and `impl Trait for char { .. }`: the
        // program's own definitions.
        if *kind == TokenKind::Keyword(Keyword::Impl)
            && let Some((ty, open)) = impl_header_type(&tokens, i + 1)
        {
            let mut depth = 0usize;
            let mut j = open;
            while j < tokens.len() {
                match &tokens[j] {
                    (TokenKind::Punct(Punct::LBrace), _) => depth += 1,
                    (TokenKind::Punct(Punct::RBrace), _) => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    (TokenKind::Keyword(Keyword::Fn), _) if depth == 1 => {
                        if let Some((TokenKind::Ident, name)) = tokens.get(j + 1) {
                            defined.insert((ty.clone(), name.clone()));
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            i = j;
        }
        i += 1;
    }
    Mentions { called, defined }
}

/// The tokens of `source` that are code: an f-string's interpolations are
/// code too, so their tokens follow the literal's.
fn code_tokens(source: &str, tokens: &mut Vec<(TokenKind, String)>) {
    let mut map = SourceMap::new();
    let file = map.add_file("<primitive-surface-scan>", String::new());
    let mut lexer = Lexer::new(source, file);
    loop {
        let token = lexer.next_token();
        if token.kind == TokenKind::Eof {
            break;
        }
        if matches!(
            token.kind,
            TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment
        ) {
            continue;
        }
        let text = source
            .get(token.span.start as usize..token.span.end as usize)
            .unwrap_or("")
            .to_string();
        if token.kind == TokenKind::FStringLit {
            // `f"text {expr} text"`: the text between the quotes, read as
            // code; the literal pieces only add names nothing calls.
            let inner = text.trim_start_matches('f').trim_matches('"').to_string();
            code_tokens(&inner, tokens);
        }
        tokens.push((token.kind, text));
    }
}

/// The self type of the `impl` header starting at `at` when it is a bare
/// name, with the index of the `{` opening the block. The name after `for`
/// is the self type of a trait impl.
fn impl_header_type(tokens: &[(TokenKind, String)], at: usize) -> Option<(String, usize)> {
    let open = at
        + tokens[at..]
            .iter()
            .position(|(kind, _)| matches!(kind, TokenKind::Punct(Punct::LBrace | Punct::Semi)))?;
    if !matches!(tokens[open].0, TokenKind::Punct(Punct::LBrace)) {
        return None;
    }
    let header = &tokens[at..open];
    let self_ty = match header
        .iter()
        .position(|(kind, _)| *kind == TokenKind::Keyword(Keyword::For))
    {
        Some(for_at) => &header[for_at + 1..],
        None => header,
    };
    match self_ty {
        [(TokenKind::Ident, name)] => Some((name.clone(), open)),
        _ => None,
    }
}

/// Gossamer source declaring the primitive methods `source` names, or an
/// empty string when it names none.
#[must_use]
pub(crate) fn synthesize_primitive_surface(source: &str) -> String {
    if !table_names().any(|name| source.contains(name)) {
        return String::new();
    }
    let mentions = scan(source);
    // The requested methods, closed over the methods their bodies call.
    let mut wanted: BTreeSet<&'static str> = table_names()
        .filter(|name| mentions.called.contains(*name))
        .collect();
    loop {
        let before = wanted.len();
        let extra: Vec<&'static str> = METHODS
            .iter()
            .filter(|m| wanted.contains(m.name))
            .flat_map(|m| m.needs.iter().copied())
            .collect();
        wanted.extend(extra);
        if wanted.len() == before {
            break;
        }
    }
    if wanted.is_empty() {
        return String::new();
    }
    let mut by_type: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for m in METHODS.iter().filter(|m| wanted.contains(m.name)) {
        for ty in m.receivers.types() {
            if mentions
                .defined
                .contains(&((*ty).to_string(), m.name.to_string()))
            {
                continue;
            }
            // A generic receiver keeps its `T` as the impl's own parameter.
            if *ty == "Vec" {
                by_type.entry(ty).or_default().push(m.source.to_string());
                continue;
            }
            let body = m
                .source
                .replace("(T,", &format!("({ty},"))
                .replace("T::", &format!("{ty}::"))
                .replace(": T", &format!(": {ty}"))
                .replace("-> T", &format!("-> {ty}"))
                .replace("<T>", &format!("<{ty}>"))
                .replace("as U", &format!("as {}", unsigned_of(ty)))
                .replace("as T", &format!("as {ty}"))
                .replace("-> U", &format!("-> {}", unsigned_of(ty)));
            by_type.entry(ty).or_default().push(body);
        }
    }
    let mut out = String::from("\n");
    if by_type.contains_key("char") {
        out.push_str(CHAR_HELPERS);
    }
    for (ty, methods) in by_type {
        if ty == "Vec" {
            out.push_str("impl<T> Vec<T> {\n");
        } else {
            out.push_str(&format!("impl {ty} {{\n"));
        }
        for body in methods {
            out.push_str("    ");
            out.push_str(&body);
            out.push('\n');
        }
        out.push_str("}\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::synthesize_primitive_surface;

    #[test]
    fn a_trait_impl_on_a_primitive_keeps_its_own_method() {
        let source = "trait P { fn pow(&self, e: i64) -> i64 }\n\
                      impl P for i64 { fn pow(&self, e: i64) -> i64 { e } }\n\
                      fn main() { let x = 2.pow(3) }";
        assert!(!synthesize_primitive_surface(source).contains("impl i64"));
    }

    #[test]
    fn a_program_naming_no_method_gets_no_surface() {
        assert!(synthesize_primitive_surface("fn main() { let x = 1 }").is_empty());
    }

    #[test]
    fn a_named_method_is_declared_with_the_methods_it_calls() {
        let out = synthesize_primitive_surface("fn main() { let _ = 5.saturating_add(3) }");
        assert!(out.contains("impl i64 {"));
        assert!(out.contains("fn saturating_add(&self, rhs: i64) -> i64"));
        assert!(out.contains("fn checked_add(&self, rhs: i64) -> Option<i64>"));
        assert!(out.contains("impl u8 {"));
        assert!(!out.contains("fn checked_mul"));
    }

    #[test]
    fn a_method_the_program_defines_is_left_to_it() {
        let src = "impl char { fn is_alphabetic(&self) -> bool { true } }\n\
                   fn main() { let _ = 'a'.is_alphabetic() }";
        let out = synthesize_primitive_surface(src);
        assert!(!out.contains("fn is_alphabetic"), "{out}");
    }

    #[test]
    fn char_methods_reach_unicode_through_the_helper_module() {
        let out = synthesize_primitive_surface("fn main() { let _ = 'a'.is_alphabetic() }");
        assert!(out.contains("mod __gos_prim_char"));
        assert!(out.contains("impl char {"));
    }
}
