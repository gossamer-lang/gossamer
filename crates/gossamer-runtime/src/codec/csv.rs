//! Comma-separated values (RFC 4180): `"` quotes a field, `""` is a quote
//! inside one, and a quoted field may hold commas and line breaks.

/// The fields of one CSV line. A line break in `line` is field text.
#[must_use]
pub fn parse_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut scratch = String::new();
    for_each_field(line, &mut scratch, |f| {
        fields.push(String::from_utf8_lossy(f).into_owned());
    });
    fields
}

/// Every record of a CSV document. Blank lines between records are skipped,
/// and a record ends at an unquoted `\n` or `\r\n`.
pub fn read(input: &str) -> Result<Vec<Vec<String>>, String> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut scratch = String::new();
    for_each_record(input, &mut scratch, |event| match event {
        Event::Field(f) => record.push(String::from_utf8_lossy(f).into_owned()),
        Event::EndRecord => records.push(std::mem::take(&mut record)),
    })?;
    Ok(records)
}

/// One step of a walk over a CSV document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// The next field of the current record, unquoted.
    Field(&'a [u8]),
    /// The current record has no more fields.
    EndRecord,
}

/// Walks the records of a CSV document, handing `visit` each field and the
/// end of each record in order.
pub fn for_each_record(
    input: &str,
    scratch: &mut String,
    mut visit: impl FnMut(Event<'_>),
) -> Result<(), String> {
    let bytes = input.as_bytes();
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
        for_each_field(record, scratch, |f| visit(Event::Field(f)));
        visit(Event::EndRecord);
    }
    Ok(())
}

/// Splits one record into fields, handing each to `emit`.
///
/// A field carrying no quote is a run of the record itself and reaches
/// `emit` as that run, so the common field costs no copy. Only a quoted
/// field is assembled, into `scratch`, which the caller reuses across the
/// whole document. The separator and the quote are ASCII, so a byte can be
/// one of them only where a character is.
fn for_each_field(record: &str, scratch: &mut String, mut emit: impl FnMut(&[u8])) {
    let bytes = record.as_bytes();
    let mut assembling = false;
    let mut in_quotes = false;
    let mut run_start = 0usize;
    let mut i = 0usize;
    scratch.clear();
    while i < bytes.len() {
        match bytes[i] {
            b'"' if in_quotes => {
                scratch.push_str(&record[run_start..i]);
                assembling = true;
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
                scratch.push_str(&record[run_start..i]);
                assembling = true;
                in_quotes = true;
                i += 1;
                run_start = i;
            }
            b',' if !in_quotes => {
                if assembling {
                    scratch.push_str(&record[run_start..i]);
                    emit(scratch.as_bytes());
                    scratch.clear();
                    assembling = false;
                } else {
                    emit(&bytes[run_start..i]);
                }
                i += 1;
                run_start = i;
            }
            _ => i += 1,
        }
    }
    if assembling {
        scratch.push_str(&record[run_start..]);
        emit(scratch.as_bytes());
    } else {
        emit(&bytes[run_start..]);
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
    fn unterminated_quote_is_refused() {
        assert_eq!(
            read("a,b\nc,\"d\ne"),
            Err("csv: unterminated quoted field in: c,\"d".to_string())
        );
    }
}
