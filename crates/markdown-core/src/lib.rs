//! Pure markdown model for a WYSIWYG editor.
//!
//! Parses a document into [`Block`]s, each holding one or more
//! [`RenderLine`]s, each a sequence of styled [`Run`]s. The unit of
//! layout is the *render line* — one source line. Markdown keeps its
//! source-line structure (we do not reflow soft-wrapped paragraphs into
//! one), which makes raw editing intuitive and, crucially, gives **exact
//! source char-offset coverage**: the concatenation of every run's
//! `src` range, in document order, partitions the document's characters
//! exactly. A WYSIWYG renderer relies on that to map every caret offset
//! back to a rendered glyph.
//!
//! All offsets are **character** offsets (not bytes), matching the
//! editor's `ropey`/selection model.
//!
//! Inline parsing (emphasis, code spans, strikethrough, links) is a
//! pragmatic scanner, not full CommonMark: it favours predictable,
//! total coverage over spec-perfect matching. Visual fidelity is
//! approximate on adversarial input; caret correctness is exact.

use std::ops::Range;

/// Inline text styling applied to a run. Combines with the block kind
/// (a heading run is also large; a code-block run is also mono).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    /// Inline code span (`` `like this` ``) — rendered mono.
    pub code: bool,
    pub strike: bool,
    /// Link text — rendered in the link color (and underlined later).
    pub link: bool,
}

/// What a run *is*, for hide/reveal purposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    /// Visible content. Always rendered (with its [`InlineStyle`]).
    Text(InlineStyle),
    /// Markdown syntax (`#`, `**`, `` ` ``, `>`, `-`, fences, link
    /// brackets/urls). Hidden when its block is inactive; revealed
    /// dimmed when the caret is inside the block.
    Marker,
}

impl RunKind {
    pub fn is_marker(self) -> bool {
        matches!(self, RunKind::Marker)
    }
    pub fn style(self) -> InlineStyle {
        match self {
            RunKind::Text(s) => s,
            RunKind::Marker => InlineStyle::default(),
        }
    }
}

/// A contiguous run of characters sharing one [`RunKind`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// Text to render. For markers this is usually the literal source
    /// text; it may differ from the source slice when we substitute a
    /// glyph (e.g. a list bullet) — but `src.len()` source chars are
    /// always accounted for here.
    pub text: String,
    /// Source char range this run represents (half-open).
    pub src: Range<usize>,
    pub kind: RunKind,
}

/// One source line, broken into runs. `src` is the line's content char
/// range, excluding the trailing newline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderLine {
    pub runs: Vec<Run>,
    pub src: Range<usize>,
}

impl RenderLine {
    /// True if `offset` falls within this line's content (inclusive of
    /// the end, so an end-of-line caret counts).
    pub fn contains(&self, offset: usize) -> bool {
        offset >= self.src.start && offset <= self.src.end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph,
    /// A blank source line. Renders as an empty line with normal height
    /// so the caret can sit on it and so it provides paragraph spacing.
    Blank,
    Heading(u8), // 1..=6
    CodeBlock,
    BlockQuote,
    ListItem { ordered: bool },
    ThematicBreak,
}

/// A leaf block: one or more render lines plus styling context. Most
/// blocks hold a single line; a fenced code block holds its fence lines
/// and code lines together so the renderer can draw one background.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    /// Nesting depth (blockquote / list indentation), 0 at the margin.
    pub indent: u8,
    pub lines: Vec<RenderLine>,
    /// Whole-block source char range.
    pub src: Range<usize>,
    /// Code fence info string (language), if any.
    pub lang: Option<String>,
}

impl Block {
    /// True if `caret` falls anywhere within this block's source range.
    pub fn active_for(&self, caret: usize) -> bool {
        caret >= self.src.start && caret <= self.src.end
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub blocks: Vec<Block>,
}

impl Document {
    /// All render lines in document order, flattened across blocks. Each
    /// item is `(block_index, line)`.
    pub fn render_lines(&self) -> impl Iterator<Item = (usize, &RenderLine)> {
        self.blocks
            .iter()
            .enumerate()
            .flat_map(|(bi, b)| b.lines.iter().map(move |l| (bi, l)))
    }
}

// ---------- Line splitting ----------

struct LineSpan {
    /// Char index of the first character of the line.
    start: usize,
    /// Line text with the trailing '\n' stripped.
    text: String,
}

/// Split source into lines, tracking each line's starting char offset.
/// A trailing newline yields a final empty line (matching the editor's
/// "a\n" == 2 lines convention), so caret offsets line up.
fn split_lines(source: &str) -> Vec<LineSpan> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut idx = 0usize;
    let mut buf = String::new();
    for ch in source.chars() {
        if ch == '\n' {
            lines.push(LineSpan {
                start,
                text: std::mem::take(&mut buf),
            });
            idx += 1;
            start = idx;
        } else {
            buf.push(ch);
            idx += 1;
        }
    }
    lines.push(LineSpan { start, text: buf });
    lines
}

// ---------- Public entry ----------

/// Parse a markdown document into the block/line/run model.
pub fn parse(source: &str) -> Document {
    let lines = split_lines(source);
    let mut blocks: Vec<Block> = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        let line = &lines[i];
        let trimmed_start = leading_ws(&line.text);
        let body = &line.text[byte_of_char(&line.text, trimmed_start)..];

        // Fenced code block: ``` or ~~~ (>=3) after optional indent.
        if let Some((fence_char, fence_len)) = fence_marker(body) {
            let block = consume_code_block(&lines, &mut i, fence_char, fence_len, trimmed_start);
            blocks.push(block);
            continue;
        }

        // Single-line blocks below.
        let block = classify_line(line, trimmed_start, body);
        blocks.push(block);
        i += 1;
    }

    Document { blocks }
}

/// Count leading spaces (in chars). Tabs count as one for simplicity.
fn leading_ws(text: &str) -> usize {
    text.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

/// Byte offset of the `n`-th char (for slicing `&str`).
fn byte_of_char(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(b, _)| b).unwrap_or(s.len())
}

/// If `body` opens/closes a code fence, return `(fence_char, run_len)`.
fn fence_marker(body: &str) -> Option<(char, usize)> {
    let first = body.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let run = body.chars().take_while(|c| *c == first).count();
    if run >= 3 { Some((first, run)) } else { None }
}

/// Consume a fenced code block starting at `lines[*i]` (the opening
/// fence). Advances `*i` past the closing fence (or to EOF). Produces a
/// single [`BlockKind::CodeBlock`] containing the fence lines (markers)
/// and the code content lines.
fn consume_code_block(
    lines: &[LineSpan],
    i: &mut usize,
    fence_char: char,
    fence_len: usize,
    indent: usize,
) -> Block {
    let open = &lines[*i];
    let mut rlines: Vec<RenderLine> = Vec::new();
    let lang = {
        let body = &open.text[byte_of_char(&open.text, indent + fence_len)..];
        let lang = body.trim().to_string();
        if lang.is_empty() { None } else { Some(lang) }
    };
    // Opening fence line — entirely a marker.
    rlines.push(whole_line_marker(open));
    let block_start = open.start;
    let mut block_end = line_end(open);
    *i += 1;

    while *i < lines.len() {
        let l = &lines[*i];
        let ts = leading_ws(&l.text);
        let lbody = &l.text[byte_of_char(&l.text, ts)..];
        // Closing fence: same char, run >= open len, nothing else.
        if let Some((c, run)) = fence_marker(lbody) {
            if c == fence_char && run >= fence_len && lbody.chars().skip(run).all(|c| c == ' ') {
                rlines.push(whole_line_marker(l));
                block_end = line_end(l);
                *i += 1;
                break;
            }
        }
        // Code content line: one mono Text run covering the whole line.
        rlines.push(code_content_line(l));
        block_end = line_end(l);
        *i += 1;
    }

    Block {
        kind: BlockKind::CodeBlock,
        indent: (indent / 2) as u8,
        lines: rlines,
        src: block_start..block_end,
        lang,
    }
}

/// Char offset just past the line's content (before its newline).
fn line_end(l: &LineSpan) -> usize {
    l.start + l.text.chars().count()
}

/// A render line that is entirely one marker run (fences, thematic
/// breaks).
fn whole_line_marker(l: &LineSpan) -> RenderLine {
    let end = line_end(l);
    let runs = if l.text.is_empty() {
        Vec::new()
    } else {
        vec![Run {
            text: l.text.clone(),
            src: l.start..end,
            kind: RunKind::Marker,
        }]
    };
    RenderLine {
        runs,
        src: l.start..end,
    }
}

/// A code-block content line: one mono Text run, no inline parsing.
fn code_content_line(l: &LineSpan) -> RenderLine {
    let end = line_end(l);
    let runs = if l.text.is_empty() {
        Vec::new()
    } else {
        vec![Run {
            text: l.text.clone(),
            src: l.start..end,
            kind: RunKind::Text(InlineStyle {
                code: true,
                ..Default::default()
            }),
        }]
    };
    RenderLine {
        runs,
        src: l.start..end,
    }
}

/// Classify a single non-fence line into a one-line block.
fn classify_line(line: &LineSpan, indent: usize, body: &str) -> Block {
    let end = line_end(line);
    let src = line.start..end;

    // Blank line.
    if body.trim().is_empty() {
        let runs = if line.text.is_empty() {
            Vec::new()
        } else {
            // Whitespace-only: keep as plain text so chars are covered.
            vec![Run {
                text: line.text.clone(),
                src: line.start..end,
                kind: RunKind::Text(InlineStyle::default()),
            }]
        };
        return Block {
            kind: BlockKind::Blank,
            indent: 0,
            lines: vec![RenderLine { runs, src: src.clone() }],
            src,
            lang: None,
        };
    }

    // Thematic break: >=3 of -, *, or _ (with optional spaces), nothing else.
    if is_thematic_break(body) {
        return Block {
            kind: BlockKind::ThematicBreak,
            indent: 0,
            lines: vec![whole_line_marker(line)],
            src: src.clone(),
            lang: None,
        };
    }

    // ATX heading.
    if let Some(level) = atx_heading_level(body) {
        // marker = indent + hashes + following spaces; content = rest.
        let hashes = level as usize;
        let after_hashes = body.chars().skip(hashes).take_while(|c| *c == ' ').count();
        let marker_chars = indent + hashes + after_hashes;
        let content_start = line.start + marker_chars;
        let marker_text: String = line.text.chars().take(marker_chars).collect();
        let content: String = line.text.chars().skip(marker_chars).collect();
        let mut runs = Vec::new();
        if !marker_text.is_empty() {
            runs.push(Run {
                text: marker_text,
                src: line.start..content_start,
                kind: RunKind::Marker,
            });
        }
        runs.extend(parse_inline(&content, content_start));
        return Block {
            kind: BlockKind::Heading(level),
            indent: 0,
            lines: vec![RenderLine { runs, src: src.clone() }],
            src,
            lang: None,
        };
    }

    // Blockquote: leading '>' (after optional indent).
    if body.starts_with('>') {
        let after = &body[1..];
        let space = if after.starts_with(' ') { 1 } else { 0 };
        let marker_chars = indent + 1 + space;
        let content_start = line.start + marker_chars;
        let marker_text: String = line.text.chars().take(marker_chars).collect();
        let content: String = line.text.chars().skip(marker_chars).collect();
        let mut runs = vec![Run {
            text: marker_text,
            src: line.start..content_start,
            kind: RunKind::Marker,
        }];
        runs.extend(parse_inline(&content, content_start));
        return Block {
            kind: BlockKind::BlockQuote,
            indent: (indent / 2) as u8,
            lines: vec![RenderLine { runs, src: src.clone() }],
            src,
            lang: None,
        };
    }

    // List item: bullet (-,*,+) or ordered (N. / N)) then a space.
    if let Some((marker_len, ordered)) = list_marker(body) {
        let marker_chars = indent + marker_len;
        let content_start = line.start + marker_chars;
        let marker_text: String = line.text.chars().take(marker_chars).collect();
        let content: String = line.text.chars().skip(marker_chars).collect();
        let mut runs = vec![Run {
            text: marker_text,
            src: line.start..content_start,
            kind: RunKind::Marker,
        }];
        runs.extend(parse_inline(&content, content_start));
        return Block {
            kind: BlockKind::ListItem { ordered },
            indent: (indent / 2) as u8,
            lines: vec![RenderLine { runs, src: src.clone() }],
            src,
            lang: None,
        };
    }

    // Plain paragraph line — inline parse the whole thing.
    let runs = parse_inline(&line.text, line.start);
    Block {
        kind: BlockKind::Paragraph,
        indent: 0,
        lines: vec![RenderLine { runs, src: src.clone() }],
        src,
        lang: None,
    }
}

fn is_thematic_break(body: &str) -> bool {
    let mut kind: Option<char> = None;
    let mut count = 0;
    for c in body.chars() {
        match c {
            ' ' | '\t' => {}
            '-' | '*' | '_' => {
                if let Some(k) = kind {
                    if k != c {
                        return false;
                    }
                } else {
                    kind = Some(c);
                }
                count += 1;
            }
            _ => return false,
        }
    }
    count >= 3
}

fn atx_heading_level(body: &str) -> Option<u8> {
    let hashes = body.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    // Must be followed by a space. (We intentionally do NOT treat a bare
    // `#` at end-of-line as a heading: in the live WYSIWYG editor that
    // would convert the line the instant you type `#`, before you've
    // typed the space — jarring. Require the space.)
    match body.chars().nth(hashes) {
        Some(' ') => Some(hashes as u8),
        _ => None,
    }
}

/// Returns `(marker_len_in_chars, ordered)` if `body` opens a list item.
/// `marker_len` counts the bullet/number, its punctuation, and the one
/// following space.
fn list_marker(body: &str) -> Option<(usize, bool)> {
    let mut chars = body.chars();
    let first = chars.next()?;
    if matches!(first, '-' | '*' | '+') {
        if body.chars().nth(1) == Some(' ') {
            return Some((2, false));
        }
        return None;
    }
    if first.is_ascii_digit() {
        let digits = body.chars().take_while(|c| c.is_ascii_digit()).count();
        let punct = body.chars().nth(digits);
        if matches!(punct, Some('.') | Some(')')) && body.chars().nth(digits + 1) == Some(' ') {
            return Some((digits + 2, true));
        }
    }
    None
}

// ---------- Inline scanner ----------

/// Parse inline markup in `text` (a line's content). `base` is the
/// absolute char offset of `text[0]`. Returns runs covering exactly
/// `[base, base + text.chars().count())`.
pub fn parse_inline(text: &str, base: usize) -> Vec<Run> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n == 0 {
        return Vec::new();
    }
    let mut style = vec![InlineStyle::default(); n];
    let mut marker = vec![false; n];
    let mut code = vec![false; n]; // protected from emphasis scanning

    // 1) Code spans: backtick runs of equal length.
    let mut i = 0;
    while i < n {
        if chars[i] == '`' {
            let run = (i..n).take_while(|&k| chars[k] == '`').count();
            // find matching closer of the same length
            let mut j = i + run;
            let mut closed = None;
            while j < n {
                if chars[j] == '`' {
                    let crun = (j..n).take_while(|&k| chars[k] == '`').count();
                    if crun == run {
                        closed = Some(j);
                        break;
                    }
                    j += crun;
                } else {
                    j += 1;
                }
            }
            if let Some(close) = closed {
                for k in i..i + run {
                    marker[k] = true;
                    code[k] = true;
                }
                for k in i + run..close {
                    style[k].code = true;
                    code[k] = true;
                }
                for k in close..close + run {
                    marker[k] = true;
                    code[k] = true;
                }
                i = close + run;
                continue;
            }
            // No closer — treat backticks as literal text.
            i += run;
            continue;
        }
        i += 1;
    }

    // 2) Links: [text](url). Minimal, non-nested.
    let mut i = 0;
    while i < n {
        if chars[i] == '[' && !code[i] {
            if let Some(close_br) = find_char(&chars, i + 1, ']', &code) {
                if close_br + 1 < n && chars[close_br + 1] == '(' {
                    if let Some(close_par) = find_char(&chars, close_br + 2, ')', &code) {
                        // [ and ](...) are markers; inner text gets link style.
                        marker[i] = true;
                        for k in i + 1..close_br {
                            style[k].link = true;
                        }
                        for k in close_br..=close_par {
                            marker[k] = true;
                            code[k] = true; // protect url from emphasis
                        }
                        i = close_par + 1;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }

    // 3) Emphasis / strong / strikethrough via a simple delimiter stack.
    struct Delim {
        ch: char,
        len: usize,
        start: usize, // char index of the run start
    }
    let mut stack: Vec<Delim> = Vec::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if code[i] || (c != '*' && c != '_' && c != '~') {
            i += 1;
            continue;
        }
        let run = (i..n).take_while(|&k| chars[k] == c && !code[k]).count();
        let eff = if c == '~' { run.min(2) } else { run.min(3) };
        // '~' only meaningful as a pair.
        if c == '~' && eff < 2 {
            i += run;
            continue;
        }
        // Try to match an opener of the same char & length on the stack.
        if let Some(pos) = stack.iter().rposition(|d| d.ch == c && d.len == eff) {
            let opener = stack.drain(pos..).next().unwrap();
            // Everything above `pos` is discarded (unmatched) — fine.
            let content_lo = opener.start + opener.len;
            let content_hi = i;
            apply_emphasis(c, eff, &mut style, content_lo, content_hi);
            for k in opener.start..opener.start + opener.len {
                marker[k] = true;
            }
            for k in i..i + eff {
                marker[k] = true;
            }
            i += eff;
        } else {
            stack.push(Delim {
                ch: c,
                len: eff,
                start: i,
            });
            i += eff;
        }
    }

    // 4) RLE into runs.
    rle_runs(&chars, &style, &marker, base)
}

fn apply_emphasis(ch: char, eff: usize, style: &mut [InlineStyle], lo: usize, hi: usize) {
    for s in &mut style[lo..hi] {
        match (ch, eff) {
            ('~', _) => s.strike = true,
            (_, 1) => s.italic = true,
            (_, 2) => s.bold = true,
            (_, 3) => {
                s.bold = true;
                s.italic = true;
            }
            _ => {}
        }
    }
}

fn find_char(chars: &[char], from: usize, target: char, protected: &[bool]) -> Option<usize> {
    (from..chars.len()).find(|&k| chars[k] == target && !protected[k])
}

/// Run-length encode the per-char style/marker arrays into [`Run`]s,
/// covering `[base, base + chars.len())`.
fn rle_runs(chars: &[char], style: &[InlineStyle], marker: &[bool], base: usize) -> Vec<Run> {
    let n = chars.len();
    let mut out: Vec<Run> = Vec::new();
    let mut i = 0;
    while i < n {
        let m = marker[i];
        let s = style[i];
        let mut j = i + 1;
        while j < n && marker[j] == m && style[j] == s {
            j += 1;
        }
        let text: String = chars[i..j].iter().collect();
        out.push(Run {
            text,
            src: base + i..base + j,
            kind: if m { RunKind::Marker } else { RunKind::Text(s) },
        });
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The core invariant: every source char is covered exactly once,
    /// in order, by the flattened runs (newlines excepted — they are the
    /// gaps between line `src.end` and the next line `src.start`).
    fn assert_full_coverage(source: &str) {
        let doc = parse(source);
        let mut cursor = 0usize;
        for (_, line) in doc.render_lines() {
            // gap from cursor to line.start must be exactly the newlines.
            assert_eq!(line.src.start, cursor, "line start gap in {source:?}");
            let mut c = line.src.start;
            for run in &line.runs {
                assert_eq!(run.src.start, c, "run gap in {source:?}: {run:?}");
                assert!(run.src.end >= run.src.start);
                c = run.src.end;
            }
            assert_eq!(c, line.src.end, "line not fully covered in {source:?}");
            // next line starts one past (the newline) — or equal if no nl.
            cursor = line.src.end + 1;
        }
        // total chars accounted for (minus the phantom final newline).
        let total = source.chars().count();
        assert!(cursor == total + 1 || cursor == total, "coverage tail {source:?}: {cursor} vs {total}");
    }

    #[test]
    fn coverage_holds_for_varied_docs() {
        for s in [
            "",
            "hello",
            "hello\n",
            "# Heading\n\nbody **bold** and *em* text\n",
            "> a quote\n- item one\n1. first\n",
            "```rust\nlet x = 1;\n```\n",
            "a `code span` b\n",
            "[link](http://x) after\n",
            "***wow*** and ~~strike~~\n",
            "---\n",
            "trailing spaces   \n",
        ] {
            assert_full_coverage(s);
        }
    }

    #[test]
    fn heading_marker_and_content() {
        let doc = parse("## Title");
        assert_eq!(doc.blocks.len(), 1);
        let b = &doc.blocks[0];
        assert_eq!(b.kind, BlockKind::Heading(2));
        assert_eq!(b.lines[0].runs[0].kind, RunKind::Marker);
        assert_eq!(b.lines[0].runs[0].text, "## ");
        assert_eq!(b.lines[0].runs[1].text, "Title");
    }

    #[test]
    fn bold_and_italic_runs() {
        let doc = parse("a **b** c");
        let runs = &doc.blocks[0].lines[0].runs;
        // "a " text, "**" marker, "b" bold, "**" marker, " c" text
        let bold: Vec<_> = runs
            .iter()
            .filter(|r| matches!(r.kind, RunKind::Text(s) if s.bold))
            .collect();
        assert_eq!(bold.len(), 1);
        assert_eq!(bold[0].text, "b");
        let markers: Vec<_> = runs.iter().filter(|r| r.kind.is_marker()).collect();
        assert_eq!(markers.len(), 2);
        assert!(markers.iter().all(|m| m.text == "**"));
    }

    #[test]
    fn code_span_protects_asterisks() {
        let doc = parse("`a*b*c`");
        let runs = &doc.blocks[0].lines[0].runs;
        // backticks are markers, inner is code, no emphasis applied.
        let code: Vec<_> = runs
            .iter()
            .filter(|r| matches!(r.kind, RunKind::Text(s) if s.code))
            .collect();
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].text, "a*b*c");
    }

    #[test]
    fn fenced_code_block_groups_lines() {
        let doc = parse("```\nx\ny\n```\n");
        // one code block (plus a trailing blank line block).
        let code = doc
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::CodeBlock)
            .unwrap();
        assert_eq!(code.lines.len(), 4); // open, x, y, close
        assert!(code.lines[0].runs[0].kind.is_marker());
        assert!(matches!(code.lines[1].runs[0].kind, RunKind::Text(s) if s.code));
    }

    #[test]
    fn list_and_quote() {
        let doc = parse("- item\n> quote\n");
        assert_eq!(doc.blocks[0].kind, BlockKind::ListItem { ordered: false });
        assert_eq!(doc.blocks[0].lines[0].runs[0].text, "- ");
        assert_eq!(doc.blocks[1].kind, BlockKind::BlockQuote);
        assert_eq!(doc.blocks[1].lines[0].runs[0].text, "> ");
    }

    #[test]
    fn link_text_styled_url_hidden() {
        let doc = parse("see [home](http://x)");
        let runs = &doc.blocks[0].lines[0].runs;
        let link: Vec<_> = runs
            .iter()
            .filter(|r| matches!(r.kind, RunKind::Text(s) if s.link))
            .collect();
        assert_eq!(link.len(), 1);
        assert_eq!(link[0].text, "home");
    }
}
