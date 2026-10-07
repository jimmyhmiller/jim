//! The spans a double- or triple-click selects: the word or the line
//! under a position. Pure functions over the rope, in char offsets.

use ropey::Rope;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Blank,
    Newline,
    Punct,
}

fn class(c: char) -> Class {
    match c {
        '\n' | '\r' => Class::Newline,
        ' ' | '\t' => Class::Blank,
        c if c.is_alphanumeric() || c == '_' => Class::Word,
        _ => Class::Punct,
    }
}

/// The word at `pos`: a run of word characters, a run of blanks, or a
/// single punctuation character. Looks at the character after `pos`,
/// falling back to the one before when `pos` sits at a line end — so a
/// double-click past the last word of a line selects that word. Empty
/// (`pos..pos`) on an empty line.
pub fn word_range_at(doc: &Rope, pos: usize) -> (usize, usize) {
    let len = doc.len_chars();
    let pos = pos.min(len);
    let idx = if pos < len && class(doc.char(pos)) != Class::Newline {
        pos
    } else if pos > 0 && class(doc.char(pos - 1)) != Class::Newline {
        pos - 1
    } else {
        return (pos, pos);
    };
    let cls = class(doc.char(idx));
    if cls == Class::Punct {
        return (idx, idx + 1);
    }
    let mut from = idx;
    while from > 0 && class(doc.char(from - 1)) == cls {
        from -= 1;
    }
    let mut to = idx + 1;
    while to < len && class(doc.char(to)) == cls {
        to += 1;
    }
    (from, to)
}

/// The whole line containing `pos`, including its trailing newline (so a
/// triple-click selection deletes or copies a complete line).
pub fn line_range_at(doc: &Rope, pos: usize) -> (usize, usize) {
    let pos = pos.min(doc.len_chars());
    let line = doc.char_to_line(pos);
    let from = doc.line_to_char(line);
    let to = if line + 1 < doc.len_lines() {
        doc.line_to_char(line + 1)
    } else {
        doc.len_chars()
    };
    (from, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, pos: usize) -> &str {
        let doc = Rope::from_str(text);
        let (a, b) = word_range_at(&doc, pos);
        let (a, b) = (doc.char_to_byte(a), doc.char_to_byte(b));
        &text[a..b]
    }

    #[test]
    fn word_under_caret() {
        assert_eq!(word("let foo_bar = 1;", 5), "foo_bar");
        assert_eq!(word("let foo_bar = 1;", 4), "foo_bar");
        assert_eq!(word("let foo_bar = 1;", 0), "let");
    }

    #[test]
    fn line_end_takes_the_word_before() {
        assert_eq!(word("one two\nthree", 7), "two");
        assert_eq!(word("one two", 7), "two");
    }

    #[test]
    fn blanks_and_punctuation() {
        assert_eq!(word("a    b", 2), "    ");
        assert_eq!(word("a::b", 1), ":");
        assert_eq!(word("héllo wörld", 8), "wörld");
    }

    #[test]
    fn empty_line_is_empty() {
        let doc = Rope::from_str("a\n\nb");
        assert_eq!(word_range_at(&doc, 2), (2, 2));
        assert_eq!(word_range_at(&Rope::from_str(""), 0), (0, 0));
    }

    #[test]
    fn line_includes_newline() {
        let doc = Rope::from_str("one\ntwo\nthree");
        assert_eq!(line_range_at(&doc, 5), (4, 8));
        assert_eq!(line_range_at(&doc, 10), (8, 13));
        assert_eq!(line_range_at(&doc, 3), (0, 4));
    }
}
