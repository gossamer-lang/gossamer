//! Pretty-printer that renders any AST back into idiomatic Gossamer source.
//! The printer owns a single `String` buffer and emits 4-space indentation,
//! brace-delimited blocks, and one `|>` per line for pipe chains of length
//! two or more. It is deliberately conservative about precedence: binary and
//! unary expressions are re-parenthesised whenever the child's precedence
//! permits a different parse.

#![forbid(unsafe_code)]

mod expr;
mod items;
mod types;

use crate::source_file::SourceFile;

/// Stateful pretty-printer for Gossamer AST nodes.
#[derive(Debug, Default, Clone)]
pub struct Printer {
    /// Accumulated output.
    buffer: String,
    /// Current indentation depth in units of 4 spaces.
    indent: u32,
    /// `true` when the current line has no characters written yet.
    line_start: bool,
}

/// Maximum columns the pretty-printer targets before it breaks an
/// argument list / struct literal / array literal across lines.
/// Matches `rustfmt.toml`'s `max_width = 100`.
pub(super) const MAX_LINE_WIDTH: usize = 100;

impl Printer {
    /// Constructs a fresh printer with an empty output buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            indent: 0,
            line_start: true,
        }
    }

    /// Consumes the printer and returns the rendered output string.
    #[must_use]
    pub fn finish(self) -> String {
        self.buffer
    }

    /// Renders a full source file into this printer.
    pub fn print_source_file(&mut self, source: &SourceFile) {
        let mut needs_blank = false;
        if !source.uses.is_empty() {
            let mut rest = source.uses.as_slice();
            while let Some(first) = rest.first() {
                let len = match first.group {
                    Some(group) => rest
                        .iter()
                        .take_while(|decl| decl.group == Some(group))
                        .count(),
                    None => 1,
                };
                let (decls, after) = rest.split_at(len);
                self.print_use_group(decls);
                self.newline();
                rest = after;
            }
            needs_blank = true;
        }
        for (index, item) in source.items.iter().enumerate() {
            if needs_blank || index > 0 {
                self.newline();
            }
            self.print_item(item);
            self.newline();
            needs_blank = true;
        }
    }

    /// Writes a string slice, prepending indentation if we are at column zero.
    pub(super) fn write(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.line_start {
            self.write_indent();
            self.line_start = false;
        }
        self.buffer.push_str(text);
    }

    /// Ends the current line and marks the next write to emit indentation.
    pub(super) fn newline(&mut self) {
        self.buffer.push('\n');
        self.line_start = true;
    }

    /// Increases indentation depth by one 4-space unit.
    pub(super) fn indent_in(&mut self) {
        self.indent = self.indent.saturating_add(1);
    }

    /// Decreases indentation depth by one 4-space unit.
    pub(super) fn indent_out(&mut self) {
        self.indent = self.indent.saturating_sub(1);
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent {
            self.buffer.push_str("    ");
        }
    }

    /// Byte offset of the most recent newline in the buffer, or 0 if
    /// there are none yet.
    fn last_newline(&self) -> usize {
        self.buffer.rfind('\n').map_or(0, |pos| pos + 1)
    }

    /// Characters written on the line currently being emitted,
    /// including pending indentation if we are at the start of the
    /// line.
    pub(super) fn current_column(&self) -> usize {
        let tail = &self.buffer[self.last_newline()..];
        let mut cols = tail.chars().count();
        if self.line_start {
            cols += (self.indent as usize) * 4;
        }
        cols
    }

    /// Speculatively renders `f` into a fresh printer and returns
    /// the result. Callers use this to decide whether the output
    /// fits on one line or needs to be broken up.
    pub(super) fn speculative<F>(&self, f: F) -> String
    where
        F: FnOnce(&mut Printer),
    {
        let mut probe = Printer::new();
        probe.indent = self.indent;
        probe.line_start = false;
        f(&mut probe);
        probe.buffer
    }
}

#[cfg(test)]
mod tests {
    use super::Printer;

    #[test]
    fn empty_printer_produces_empty_string() {
        assert_eq!(Printer::new().finish(), "");
    }

    #[test]
    fn indent_writes_spaces_only_on_line_start() {
        let mut printer = Printer::new();
        printer.indent_in();
        printer.write("hello");
        printer.write(", world");
        printer.newline();
        printer.write("done");
        assert_eq!(printer.finish(), "    hello, world\n    done");
    }
}
