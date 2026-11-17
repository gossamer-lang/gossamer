//! Text operations every tier shares.

/// The pieces `text` splits into on `sep`, exactly as `str::split` answers
/// them. A one-byte separator is found by a plain byte scan, which costs a few
/// instructions per byte against the general pattern searchers' fixed cost per
/// match, the bulk of the work when the fields are short.
#[must_use]
pub fn split_pieces<'a>(text: &'a str, sep: &'a str) -> SplitPieces<'a> {
    match sep.as_bytes() {
        [byte] => SplitPieces::Byte(ByteSplit::new(text, *byte, usize::MAX)),
        _ => SplitPieces::Str(text.split(sep)),
    }
}

/// The pieces `text` splits into on `sep`, at most `n` of them, as
/// `str::splitn` answers them, with the same byte scan as [`split_pieces`].
#[must_use]
pub fn splitn_pieces<'a>(text: &'a str, n: usize, sep: &'a str) -> SplitNPieces<'a> {
    match sep.as_bytes() {
        [byte] => SplitNPieces::Byte(ByteSplit::new(text, *byte, n)),
        _ => SplitNPieces::Str(text.splitn(n, sep)),
    }
}

/// The pieces of a text split on one byte, at most `left` of them.
#[derive(Debug, Clone)]
pub struct ByteSplit<'a> {
    rest: Option<&'a str>,
    sep: u8,
    left: usize,
}

impl<'a> ByteSplit<'a> {
    fn new(text: &'a str, sep: u8, left: usize) -> Self {
        Self {
            rest: Some(text),
            sep,
            left,
        }
    }
}

impl<'a> Iterator for ByteSplit<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.left == 0 {
            return None;
        }
        let rest = self.rest?;
        self.left -= 1;
        if self.left == 0 {
            self.rest = None;
            return Some(rest);
        }
        // A one-byte `&str` separator is ASCII, so the byte after it starts a
        // character and both slices fall on character boundaries.
        if let Some(at) = rest.as_bytes().iter().position(|&b| b == self.sep) {
            self.rest = Some(&rest[at + 1..]);
            Some(&rest[..at])
        } else {
            self.rest = None;
            Some(rest)
        }
    }
}

/// The iterator [`split_pieces`] answers.
#[derive(Debug, Clone)]
pub enum SplitPieces<'a> {
    /// Split on a one-byte separator.
    Byte(ByteSplit<'a>),
    /// Split on any other separator.
    Str(std::str::Split<'a, &'a str>),
}

impl<'a> Iterator for SplitPieces<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        match self {
            Self::Byte(pieces) => pieces.next(),
            Self::Str(pieces) => pieces.next(),
        }
    }
}

/// The iterator [`splitn_pieces`] answers.
#[derive(Debug, Clone)]
pub enum SplitNPieces<'a> {
    /// Split on a one-byte separator.
    Byte(ByteSplit<'a>),
    /// Split on any other separator.
    Str(std::str::SplitN<'a, &'a str>),
}

impl<'a> Iterator for SplitNPieces<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        match self {
            Self::Byte(pieces) => pieces.next(),
            Self::Str(pieces) => pieces.next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same_as_std(text: &str, sep: &str) {
        let ours: Vec<&str> = split_pieces(text, sep).collect();
        let std: Vec<&str> = text.split(sep).collect();
        assert_eq!(ours, std, "split {text:?} on {sep:?}");
        for n in 0..5 {
            let ours: Vec<&str> = splitn_pieces(text, n, sep).collect();
            let std: Vec<&str> = text.splitn(n, sep).collect();
            assert_eq!(ours, std, "splitn {n} {text:?} on {sep:?}");
        }
    }

    #[test]
    fn split_pieces_answers_what_str_split_answers() {
        for text in [
            "",
            " ",
            "a b  c ",
            "1 (bash) S 1",
            "é ✓ é",
            "no-separator",
            "  ",
            "a,b,,c,",
            ",",
        ] {
            for sep in [" ", "", "  ", "é", "b", "-", ","] {
                same_as_std(text, sep);
            }
        }
    }
}
