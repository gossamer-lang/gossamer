//! Writes the UTS #39 confusable skeleton table as flat arrays.
//!
//! `unicode-security` keeps its prototypes as a table of slices, which a
//! position-independent binary relocates at every start. The lexer needs a
//! prototype only for a file with a non-ASCII identifier, so the table is
//! written here as sorted scalar keys, offsets, and one string of
//! prototypes: read-only data nothing touches until a lookup reads it.

use std::fmt::Write as _;
use std::path::PathBuf;

use unicode_normalization::UnicodeNormalization;

fn main() {
    let mut keys = String::new();
    let mut offsets = String::new();
    let mut prototypes = String::new();
    let mut count = 0usize;
    for scalar in 0..=u32::from(char::MAX) {
        let Some(c) = char::from_u32(scalar) else {
            continue;
        };
        // An identifier holds only identifier characters, and the lexer
        // decomposes a name before it looks a character up, so a composed
        // character or one no identifier can hold needs no entry.
        if !unicode_ident::is_xid_continue(c) {
            continue;
        }
        let own = c.to_string();
        if own.nfd().ne(own.chars()) {
            continue;
        }
        let skeleton: String = unicode_security::skeleton(&own).collect();
        if skeleton == own {
            continue;
        }
        let _ = write!(keys, "{scalar},");
        let _ = write!(offsets, "{},", prototypes.len());
        prototypes.push_str(&skeleton);
        count += 1;
    }
    let _ = write!(offsets, "{},", prototypes.len());
    let table = format!(
        "/// Scalars whose skeleton differs from themselves, ascending.\n\
         static KEYS: [u32; {count}] = [{keys}];\n\
         /// Byte offsets of each key's skeleton in [`PROTOTYPES`], plus the end.\n\
         static OFFSETS: [u32; {}] = [{offsets}];\n\
         /// Every key's skeleton, back to back.\n\
         static PROTOTYPES: &str = \"{}\";\n",
        count + 1,
        ascii_escaped(&prototypes)
    );
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(out.join("confusables.rs"), table).expect("OUT_DIR is writable");
    println!("cargo::rerun-if-changed=build.rs");
}

/// `text` as the body of a Rust string literal written in ASCII alone: the
/// prototypes are not in normalization form C, which a literal of the
/// characters themselves would be linted for.
fn ascii_escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ' '..='~' => out.push(c),
            _ => {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
        }
    }
    out
}
