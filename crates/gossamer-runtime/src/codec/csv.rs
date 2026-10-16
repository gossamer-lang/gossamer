//! Comma-separated values (RFC 4180): `"` quotes a field, `""` is a quote
//! inside one, and a quoted field may hold commas and line breaks.

/// The fields of one CSV line. A line break in `line` is field text.
#[must_use]
pub fn parse_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut scratch = String::new();
    let mut push = |f: &str| fields.push(f.to_owned());
    let tail = split_fields(line, 0, false, &mut scratch, &mut push);
    tail.emit_last(line, line.len(), &mut scratch, &mut push);
    fields
}

/// Every record of a CSV document. Blank lines between records are skipped,
/// and a record ends at an unquoted `\n` or `\r\n`.
pub fn read(input: &str) -> Result<Vec<Vec<String>>, String> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut scratch = String::new();
    for_each_record(input, &mut scratch, |event| match event {
        Event::Field(f) => record.push(f.to_owned()),
        Event::EndRecord => records.push(std::mem::take(&mut record)),
    })?;
    Ok(records)
}

/// One step of a walk over a CSV document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// The next field of the current record, unquoted.
    Field(&'a str),
    /// The current record has no more fields.
    EndRecord,
}

/// Walks the records of a CSV document, handing `visit` each field and the
/// end of each record in order.
///
/// The document is read once: a record's fields are split as the scan finds
/// the line break that ends it.
pub fn for_each_record(
    input: &str,
    scratch: &mut String,
    mut visit: impl FnMut(Event<'_>),
) -> Result<(), String> {
    let bytes = input.as_bytes();
    let mut start = 0usize;
    while start < bytes.len() {
        let tail = split_fields(input, start, true, scratch, &mut |f| visit(Event::Field(f)));
        if tail.open_quote {
            let first_line = input[start..].lines().next().unwrap_or_default();
            return Err(format!("csv: unterminated quoted field in: {first_line}"));
        }
        // A `\r` before the line break belongs to the break, not the field.
        let end = if bytes[..tail.end].ends_with(b"\r") {
            tail.end - 1
        } else {
            tail.end
        };
        let next = tail.end + 1;
        // A record with no separator and no quote is one run, so a record of
        // nothing but whitespace has emitted nothing yet and is skipped.
        if !tail.split && input[start..end].trim().is_empty() {
            start = next;
            continue;
        }
        tail.emit_last(input, end, scratch, &mut |f| visit(Event::Field(f)));
        visit(Event::EndRecord);
        start = next;
    }
    Ok(())
}

/// Where [`split_fields`] stopped, with the last field still to emit.
struct Tail {
    /// The index of the line break that ended the record, or the input's
    /// length.
    end: usize,
    /// Whether a quote was still open there.
    open_quote: bool,
    /// Whether the record held a separator or a quote.
    split: bool,
    /// Whether the last field is being assembled in the scratch buffer.
    assembling: bool,
    /// Where the last field's current run starts.
    run_start: usize,
}

impl Tail {
    /// Emits the last field, which ends at `end`.
    fn emit_last(&self, text: &str, end: usize, scratch: &mut String, emit: &mut impl FnMut(&str)) {
        if self.assembling {
            scratch.push_str(&text[self.run_start..end]);
            emit(scratch);
        } else {
            emit(&text[self.run_start..end]);
        }
    }
}

/// The index of the first byte at or after `from` that the field split acts
/// on - a quote, and outside quotes a separator or (when `records` is set) a
/// line break - or the text's length.
///
/// Eight bytes are tested at a time: a byte equal to `b` makes the high bit
/// of its lane set in `(x ^ b) - 0x01..01 & !(x ^ b)`, and the lowest set
/// lane is exact, since a borrow only travels toward higher lanes.
#[inline]
fn next_special(bytes: &[u8], from: usize, in_quotes: bool, records: bool) -> usize {
    const ONES: u64 = u64::from_ne_bytes([0x01; 8]);
    const HIGHS: u64 = u64::from_ne_bytes([0x80; 8]);
    let lanes_equal = |word: u64, byte: u8| {
        let x = word ^ (ONES * u64::from(byte));
        x.wrapping_sub(ONES) & !x & HIGHS
    };
    let special =
        |byte: u8| byte == b'"' || (!in_quotes && (byte == b',' || (records && byte == b'\n')));
    let mut i = from;
    while let Some(chunk) = bytes.get(i..).and_then(<[u8]>::first_chunk::<8>) {
        let word = u64::from_le_bytes(*chunk);
        let mut hits = lanes_equal(word, b'"');
        if !in_quotes {
            hits |= lanes_equal(word, b',');
            if records {
                hits |= lanes_equal(word, b'\n');
            }
        }
        if hits != 0 {
            return i + (hits.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < bytes.len() && !special(bytes[i]) {
        i += 1;
    }
    i
}

/// Splits the fields of the record starting at `start`, handing every field
/// but the last to `emit`, and stops at the end of the text or, when
/// `records` is set, at the first line break outside quotes.
///
/// A field carrying no quote is a run of the text itself and reaches `emit`
/// as that run, so the common field costs no copy. Only a quoted field is
/// assembled, into `scratch`, which the caller reuses across the whole
/// document. The separator and the quote are ASCII, so a byte can be one of
/// them only where a character is.
fn split_fields(
    text: &str,
    start: usize,
    records: bool,
    scratch: &mut String,
    emit: &mut impl FnMut(&str),
) -> Tail {
    let bytes = text.as_bytes();
    let mut assembling = false;
    let mut in_quotes = false;
    let mut split = false;
    let mut run_start = start;
    let mut i = start;
    scratch.clear();
    while i < bytes.len() {
        match bytes[i] {
            b'"' if in_quotes => {
                scratch.push_str(&text[run_start..i]);
                if bytes.get(i + 1) == Some(&b'"') {
                    scratch.push('"');
                    i += 2;
                } else {
                    in_quotes = false;
                    i += 1;
                }
                run_start = i;
            }
            b'"' => {
                scratch.push_str(&text[run_start..i]);
                assembling = true;
                split = true;
                in_quotes = true;
                i += 1;
                run_start = i;
            }
            b',' if !in_quotes => {
                if assembling {
                    scratch.push_str(&text[run_start..i]);
                    emit(scratch);
                    scratch.clear();
                    assembling = false;
                } else {
                    emit(&text[run_start..i]);
                }
                split = true;
                i += 1;
                run_start = i;
            }
            b'\n' if records && !in_quotes => break,
            _ => i = next_special(bytes, i + 1, in_quotes, records),
        }
    }
    Tail {
        end: i,
        open_quote: in_quotes,
        split,
        assembling,
        run_start,
    }
}

/// CSV text of `records`, one per line. A field holding a comma, quote, or
/// line break is quoted, with its quotes doubled, and so is a record's only
/// field when it is blank, which would otherwise write a blank line that
/// reading skips.
#[must_use]
pub fn write<R: AsRef<[F]>, F: AsRef<str>>(records: &[R]) -> String {
    let mut out = String::new();
    for (i, record) in records.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let fields = record.as_ref();
        for (j, field) in fields.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            let field = field.as_ref();
            let blank_line = fields.len() == 1 && field.trim().is_empty();
            if blank_line || field.contains([',', '"', '\n', '\r']) {
                out.push('"');
                out.push_str(&field.replace('"', "\"\""));
                out.push('"');
            } else {
                out.push_str(field);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{parse_line, read, write};

    #[test]
    fn quoted_fields_hold_separators_and_quotes() {
        assert_eq!(
            parse_line(r#"a,"b,c","d""e",,"#),
            ["a", "b,c", "d\"e", "", ""]
        );
        assert_eq!(parse_line(""), [""]);
    }

    #[test]
    fn a_record_of_one_blank_field_reads_back() {
        for records in [
            vec![vec!["a"], vec![""]],
            vec![vec!["  "]],
            vec![vec![""], vec!["b"]],
        ] {
            let text = write(&records);
            assert_eq!(
                read(&text),
                Ok(records
                    .iter()
                    .map(|r| r.iter().map(|f| (*f).to_string()).collect())
                    .collect()),
                "{text:?}"
            );
        }
        assert_eq!(
            read("tLZX\n\"\""),
            Ok(vec![vec!["tLZX".to_string()], vec![String::new()]])
        );
    }

    #[test]
    fn written_records_read_back() {
        let records = vec![
            vec!["plain".to_string(), "with,comma".to_string()],
            vec!["line\nbreak".to_string(), "quote\"d".to_string()],
            vec!["carriage\rreturn".to_string(), String::new()],
        ];
        assert_eq!(read(&write(&records)), Ok(records));
    }

    #[test]
    fn records_skip_blank_lines_and_crlf() {
        assert_eq!(
            read("a,b\r\n\r\n  \nc,\"d\r\ne\"\n"),
            Ok(vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c".to_string(), "d\r\ne".to_string()]
            ])
        );
    }

    #[test]
    fn a_quote_opened_inside_a_field_joins_the_runs_around_it() {
        assert_eq!(
            read("x\"a\"\"b\nc\"y,z\nlast"),
            Ok(vec![
                vec!["xa\"b\ncy".to_string(), "z".to_string()],
                vec!["last".to_string()]
            ])
        );
    }

    /// The record and field split read byte by byte, the way the text is
    /// specified, for comparison with the scan that skips ahead.
    fn read_bytewise(input: &str) -> Result<Vec<Vec<String>>, String> {
        let bytes = input.as_bytes();
        let mut records = Vec::new();
        let mut start = 0usize;
        while start < bytes.len() {
            let mut in_quotes = false;
            let mut end = start;
            while end < bytes.len() {
                match bytes[end] {
                    b'"' => in_quotes = !in_quotes,
                    b'\n' if !in_quotes => break,
                    _ => {}
                }
                end += 1;
            }
            if in_quotes {
                let first_line = input[start..].lines().next().unwrap_or_default();
                return Err(format!("csv: unterminated quoted field in: {first_line}"));
            }
            let record = &input[start..end];
            let record = record.strip_suffix('\r').unwrap_or(record);
            start = end + 1;
            if record.trim().is_empty() {
                continue;
            }
            let mut fields = Vec::new();
            let mut field = String::new();
            let mut quoted = false;
            let mut chars = record.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '"' if quoted && chars.peek() == Some(&'"') => {
                        field.push('"');
                        chars.next();
                    }
                    '"' => quoted = !quoted,
                    ',' if !quoted => fields.push(std::mem::take(&mut field)),
                    _ => field.push(c),
                }
            }
            fields.push(field);
            records.push(fields);
        }
        Ok(records)
    }

    #[test]
    fn the_skipping_scan_splits_as_the_bytewise_reading_does() {
        const ALPHABET: &[u8] = b"ab,,\"\n\r  xyz";
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for length in 0..48 {
            for _ in 0..200 {
                let text: String = (0..length)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        ALPHABET[(state % ALPHABET.len() as u64) as usize] as char
                    })
                    .collect();
                assert_eq!(read(&text), read_bytewise(&text), "{text:?}");
            }
        }
    }

    #[test]
    fn unterminated_quote_is_refused() {
        assert_eq!(
            read("a,b\nc,\"d\ne"),
            Err("csv: unterminated quoted field in: c,\"d".to_string())
        );
    }
}
