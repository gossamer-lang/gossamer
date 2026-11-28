//! RFC 5322 address parsing for `std::net::mail`, after Go's `net/mail`:
//! a mailbox is `Name <local@domain>` or a bare `local@domain`, a group
//! `team: a@x, b@y;` stands for its members, comments are skipped, and an
//! RFC 2047 encoded word in a display name is decoded. Local parts and
//! display names may hold UTF-8 (RFC 6532).

use serde_json::json;

/// A mailbox: the display name (empty when there is none) and the address,
/// its local part unquoted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    /// The display name, encoded words decoded.
    pub name: String,
    /// `local@domain`, a quoted local part written without its quotes.
    pub email: String,
}

/// The one mailbox `text` names, or the one member of a group.
///
/// # Errors
/// `text` is not an address, holds more than one, or is a group with no
/// member or several.
pub fn parse_address(text: &str) -> Result<Mailbox, String> {
    let mut parser = Parser::new(text);
    let mut found = parser.address(true)?;
    parser.skip_cfws()?;
    if !parser.at_end() {
        return Err(format!(
            "mail: expected a single address, found `{}`",
            parser.rest()
        ));
    }
    match found.len() {
        0 => Err("mail: empty group".to_string()),
        1 => Ok(found.remove(0)),
        _ => Err("mail: a group with more than one address".to_string()),
    }
}

/// The mailboxes of the address list `text`, groups flattened. Empty list
/// entries (`a@x, , b@y`) are skipped, as RFC 5322's obsolete syntax allows.
///
/// # Errors
/// An entry is not an address, or two entries are not separated by a comma.
pub fn parse_address_list(text: &str) -> Result<Vec<Mailbox>, String> {
    let mut parser = Parser::new(text);
    let mut list = Vec::new();
    loop {
        parser.skip_cfws()?;
        if parser.consume(',') {
            continue;
        }
        if parser.at_end() {
            break;
        }
        list.extend(parser.address(true)?);
        parser.skip_cfws()?;
        if parser.at_end() {
            break;
        }
        if !parser.consume(',') {
            return Err(format!("mail: expected a comma before `{}`", parser.rest()));
        }
    }
    if list.is_empty() {
        return Err("mail: no address".to_string());
    }
    Ok(list)
}

/// [`parse_address`] as `mail::Address` JSON.
///
/// # Errors
/// As [`parse_address`].
pub fn address_json(text: &str) -> Result<String, String> {
    parse_address(text).map(|m| json!({ "name": m.name, "email": m.email }).to_string())
}

/// [`parse_address_list`] as `mail::Address` JSON.
///
/// # Errors
/// As [`parse_address_list`].
pub fn address_list_json(text: &str) -> Result<String, String> {
    let list = parse_address_list(text)?;
    Ok(serde_json::Value::Array(
        list.into_iter()
            .map(|m| json!({ "name": m.name, "email": m.email }))
            .collect(),
    )
    .to_string())
}

fn is_atext(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~".contains(c) || !c.is_ascii()
}

fn is_wsp(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// A quoted string's content: printable ASCII but `"` and `\`, spaces,
/// and UTF-8.
fn is_qtext(c: char) -> bool {
    (c.is_ascii_graphic() && c != '"' && c != '\\') || is_wsp(c) || !c.is_ascii()
}

struct Parser<'a> {
    rest: &'a str,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self { rest: text }
    }

    fn at_end(&self) -> bool {
        self.rest.is_empty()
    }

    fn rest(&self) -> &str {
        self.rest
    }

    fn peek(&self) -> Option<char> {
        self.rest.chars().next()
    }

    fn consume(&mut self, c: char) -> bool {
        match self.rest.strip_prefix(c) {
            Some(after) => {
                self.rest = after;
                true
            }
            None => false,
        }
    }

    fn skip_space(&mut self) {
        self.rest = self
            .rest
            .trim_start_matches(|c: char| is_wsp(c) || c == '\r' || c == '\n');
    }

    /// Skips folding white space and comments.
    fn skip_cfws(&mut self) -> Result<(), String> {
        self.skip_comments().map(|_| ())
    }

    /// Skips folding white space and comments, answering the text of the
    /// last comment, which Go reads as the display name of a bare address.
    fn skip_comments(&mut self) -> Result<Option<String>, String> {
        let mut last = None;
        loop {
            self.skip_space();
            if !self.consume('(') {
                return Ok(last);
            }
            last = Some(self.comment()?);
        }
    }

    /// The text of a comment whose `(` was consumed, nested comments
    /// included as written.
    fn comment(&mut self) -> Result<String, String> {
        let mut depth = 1;
        let mut text = String::new();
        let mut chars = self.rest.char_indices();
        while let Some((at, c)) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some((_, escaped)) => text.push(escaped),
                    None => break,
                },
                '(' => {
                    depth += 1;
                    text.push(c);
                }
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        self.rest = &self.rest[at + 1..];
                        return Ok(text.trim().to_string());
                    }
                    text.push(c);
                }
                _ => text.push(c),
            }
        }
        Err("mail: unclosed comment".to_string())
    }

    /// A mailbox, or the members of a group when `groups` allows one.
    fn address(&mut self, groups: bool) -> Result<Vec<Mailbox>, String> {
        self.skip_cfws()?;
        if self.at_end() {
            return Err("mail: no address".to_string());
        }
        if self.peek() == Some('<') {
            return self.angle_addr(String::new()).map(|m| vec![m]);
        }
        // A bare addr-spec, when what follows the first token is no phrase.
        let saved = self.rest;
        let spec_error = match self.addr_spec() {
            Ok(email) => {
                let comment = self.skip_comments()?;
                if !matches!(self.peek(), Some('<' | ':')) {
                    return Ok(vec![Mailbox {
                        name: comment.unwrap_or_default(),
                        email,
                    }]);
                }
                None
            }
            Err(e) => Some(e),
        };
        self.rest = saved;
        let name = self.phrase()?;
        self.skip_cfws()?;
        if self.consume(':') {
            if !groups {
                return Err("mail: a group inside a group".to_string());
            }
            return self.group_members();
        }
        if self.peek() == Some('<') {
            return self.angle_addr(name).map(|m| vec![m]);
        }
        // With no angle-addr or group after it, the text was meant as an
        // addr-spec, and what is wrong with it is the report.
        Err(spec_error.unwrap_or_else(|| format!("mail: no angle-addr after `{name}`")))
    }

    fn angle_addr(&mut self, name: String) -> Result<Mailbox, String> {
        if !self.consume('<') {
            return Err("mail: no angle-addr".to_string());
        }
        let email = self.addr_spec()?;
        if !self.consume('>') {
            return Err(format!("mail: unclosed angle-addr before `{}`", self.rest));
        }
        Ok(Mailbox { name, email })
    }

    /// The members of a group whose `:` was consumed, through its `;`.
    fn group_members(&mut self) -> Result<Vec<Mailbox>, String> {
        let mut members = Vec::new();
        loop {
            self.skip_cfws()?;
            if self.consume(';') {
                return Ok(members);
            }
            if self.consume(',') {
                continue;
            }
            if self.at_end() {
                return Err("mail: unclosed group".to_string());
            }
            members.extend(self.address(false)?);
            self.skip_cfws()?;
            if !matches!(self.peek(), Some(',' | ';')) {
                return Err(format!(
                    "mail: expected a comma or `;` before `{}`",
                    self.rest
                ));
            }
        }
    }

    /// `local@domain`, the local part unquoted.
    fn addr_spec(&mut self) -> Result<String, String> {
        self.skip_cfws()?;
        let local = if self.peek() == Some('"') {
            self.quoted_string()?
        } else {
            self.dot_atom("local part")?
        };
        self.skip_cfws()?;
        if !self.consume('@') {
            return Err(format!("mail: missing @ after `{local}`"));
        }
        self.skip_cfws()?;
        let domain = if self.peek() == Some('[') {
            self.domain_literal()?
        } else {
            self.dot_atom("domain")?
        };
        if local.is_empty() {
            return Err("mail: empty local part".to_string());
        }
        Ok(format!("{local}@{domain}"))
    }

    /// Atoms joined by single dots, none empty.
    fn dot_atom(&mut self, what: &str) -> Result<String, String> {
        let end = self
            .rest
            .find(|c: char| !is_atext(c) && c != '.')
            .unwrap_or(self.rest.len());
        let atom = &self.rest[..end];
        if atom.is_empty() {
            return Err(format!("mail: missing {what} at `{}`", self.rest));
        }
        if atom.starts_with('.') || atom.ends_with('.') || atom.contains("..") {
            return Err(format!(
                "mail: the {what} `{atom}` has an empty dot-separated part"
            ));
        }
        self.rest = &self.rest[end..];
        Ok(atom.to_string())
    }

    fn domain_literal(&mut self) -> Result<String, String> {
        let Some(end) = self.rest.find(']') else {
            return Err("mail: unclosed domain literal".to_string());
        };
        let literal = &self.rest[..=end];
        if literal[1..end].contains(['[', '\\']) {
            return Err(format!("mail: bad domain literal `{literal}`"));
        }
        self.rest = &self.rest[end + 1..];
        Ok(literal.to_string())
    }

    /// A quoted string's content, escapes removed.
    fn quoted_string(&mut self) -> Result<String, String> {
        if !self.consume('"') {
            return Err("mail: missing quoted string".to_string());
        }
        let mut text = String::new();
        let mut chars = self.rest.char_indices();
        while let Some((at, c)) = chars.next() {
            match c {
                '"' => {
                    self.rest = &self.rest[at + 1..];
                    return Ok(text);
                }
                '\\' => match chars.next() {
                    Some((_, escaped))
                        if is_qtext(escaped) || escaped == '"' || escaped == '\\' =>
                    {
                        text.push(escaped);
                    }
                    _ => return Err("mail: bad escape in a quoted string".to_string()),
                },
                '\r' | '\n' => {}
                c if is_qtext(c) => text.push(c),
                _ => return Err(format!("mail: bad character {c:?} in a quoted string")),
            }
        }
        Err("mail: unclosed quoted string".to_string())
    }

    /// A display name: words (atoms, which may hold dots, or quoted strings)
    /// joined by single spaces, adjacent encoded words with nothing between.
    fn phrase(&mut self) -> Result<String, String> {
        let mut name = String::new();
        let mut last_encoded = false;
        loop {
            self.skip_cfws()?;
            let (word, quoted) = match self.peek() {
                Some('"') => (self.quoted_string()?, true),
                Some(c) if is_atext(c) || c == '.' => {
                    let end = self
                        .rest
                        .find(|c: char| !is_atext(c) && c != '.')
                        .unwrap_or(self.rest.len());
                    let atom = &self.rest[..end];
                    self.rest = &self.rest[end..];
                    (atom.to_string(), false)
                }
                _ => break,
            };
            let decoded = if quoted && !word.starts_with("=?") {
                None
            } else {
                decode_encoded_words(&word)?
            };
            let encoded = decoded.is_some();
            let text = decoded.unwrap_or(word);
            // Adjacent encoded words join with nothing between them.
            let joined = encoded && last_encoded;
            if !name.is_empty() && !joined {
                name.push(' ');
            }
            name.push_str(&text);
            last_encoded = encoded;
        }
        if name.is_empty() {
            return Err(format!("mail: missing a word at `{}`", self.rest));
        }
        Ok(name)
    }
}

/// The text of `word` when it is one or more RFC 2047 encoded words with
/// nothing between them, `None` when it is plain text.
fn decode_encoded_words(word: &str) -> Result<Option<String>, String> {
    if !(word.starts_with("=?") && word.ends_with("?=")) {
        return Ok(None);
    }
    let mut text = String::new();
    let mut rest = word;
    while !rest.is_empty() {
        let Some(after) = rest.strip_prefix("=?") else {
            return Ok(None);
        };
        let mut parts = after.splitn(3, '?');
        let (Some(charset), Some(encoding), Some(tail)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Ok(None);
        };
        let Some(end) = tail.find("?=") else {
            return Ok(None);
        };
        let payload = &tail[..end];
        rest = &tail[end + 2..];
        let bytes = match encoding {
            "B" | "b" => base64_simd::STANDARD
                .decode_to_vec(payload)
                .map_err(|_| format!("mail: bad base64 in the encoded word `{word}`"))?,
            "Q" | "q" => q_decode(payload)
                .ok_or_else(|| format!("mail: bad Q encoding in the encoded word `{word}`"))?,
            other => return Err(format!("mail: unknown encoded-word encoding `{other}`")),
        };
        let charset = charset
            .split('*')
            .next()
            .unwrap_or(charset)
            .to_ascii_lowercase();
        match charset.as_str() {
            "utf-8" | "utf8" | "us-ascii" | "ascii" => text.push_str(
                std::str::from_utf8(&bytes)
                    .map_err(|_| format!("mail: the encoded word `{word}` is not {charset}"))?,
            ),
            "iso-8859-1" | "latin1" | "iso_8859-1" => {
                text.extend(bytes.iter().map(|&b| char::from(b)));
            }
            other => return Err(format!("mail: unsupported charset `{other}`")),
        }
    }
    Ok(Some(text))
}

fn q_decode(payload: &str) -> Option<Vec<u8>> {
    let bytes = payload.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'_' => out.push(b' '),
            b'=' => {
                let hex = payload.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox(name: &str, email: &str) -> Mailbox {
        Mailbox {
            name: name.to_string(),
            email: email.to_string(),
        }
    }

    #[test]
    fn single_addresses_in_each_form() {
        assert_eq!(
            parse_address("ann@example.com").unwrap(),
            mailbox("", "ann@example.com")
        );
        assert_eq!(
            parse_address("Ann Lee <ann@example.com>").unwrap(),
            mailbox("Ann Lee", "ann@example.com")
        );
        assert_eq!(
            parse_address("\"Lee, Ann\" <ann@example.com>").unwrap(),
            mailbox("Lee, Ann", "ann@example.com")
        );
        assert_eq!(
            parse_address("ann@example.com (Ann Lee)").unwrap(),
            mailbox("Ann Lee", "ann@example.com")
        );
        assert_eq!(
            parse_address("<\"ann lee\"@[192.0.2.1]>").unwrap(),
            mailbox("", "ann lee@[192.0.2.1]")
        );
        assert_eq!(
            parse_address("J. Q. Public <jq@example.com>").unwrap(),
            mailbox("J. Q. Public", "jq@example.com")
        );
        assert_eq!(
            parse_address("team: ann@example.com;").unwrap(),
            mailbox("", "ann@example.com")
        );
    }

    #[test]
    fn encoded_words_decode_and_adjacent_ones_join() {
        assert_eq!(
            parse_address("=?UTF-8?Q?Ren=C3=A9e_Fox?= <r@example.com>")
                .unwrap()
                .name,
            "Renée Fox"
        );
        assert_eq!(
            parse_address("=?utf-8?b?w4lsw6lvbm9yZQ==?= <e@example.com>")
                .unwrap()
                .name,
            "Éléonore"
        );
        assert_eq!(
            parse_address("=?utf-8?q?Ren?= =?utf-8?q?=C3=A9e?= Fox <r@example.com>")
                .unwrap()
                .name,
            "Renée Fox"
        );
        assert_eq!(
            parse_address("=?ISO-8859-1?Q?Andr=E9?= <a@example.com>")
                .unwrap()
                .name,
            "André"
        );
    }

    #[test]
    fn lists_flatten_groups_and_skip_empty_entries() {
        assert_eq!(
            parse_address_list("Ann <a@x.com>, team: b@x.com, c@x.com;, , d@x.com, empty:;")
                .unwrap(),
            vec![
                mailbox("Ann", "a@x.com"),
                mailbox("", "b@x.com"),
                mailbox("", "c@x.com"),
                mailbox("", "d@x.com"),
            ]
        );
    }

    #[test]
    fn malformed_input_is_refused() {
        for bad in [
            "",
            "ann",
            "ann@",
            "@example.com",
            "Ann <ann@example.com",
            "Ann ann@example.com",
            "a@x.com b@x.com",
            "a@x.com, b@x.com",
            "team: a@x.com, b@x.com;",
            "empty:;",
            "a..b@x.com",
            "(unclosed a@x.com",
            "=?koi8-r?q?x?= <a@x.com>",
        ] {
            assert!(parse_address(bad).is_err(), "{bad:?}");
        }
        assert!(parse_address_list("a@x.com b@x.com").is_err());
        assert!(parse_address_list("team: a@x.com").is_err());
        assert!(parse_address_list(" , ").is_err());
    }
}
