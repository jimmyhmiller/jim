//! Standalone (non-Bevy) syntax highlighting for the funct `highlight`
//! host fn — the engine behind the diff / code-review widget.
//!
//! The editor pane's [`crate::jim_editor`] highlighter is a Bevy `Resource`
//! (needs `&mut`, a theme, the project palette) so it can't be called from a
//! widget worker thread. This module is the same tree-sitter machinery as a
//! pure, one-shot function: `highlight_lines(code, lang)` → one entry per
//! line, each a list of `(text, kind)` runs. The widget maps `kind` strings
//! to theme colors itself, so this stays Bevy-free and thread-safe.
//!
//! Per-language `Parser`/`Query` are cached in a `thread_local` (one widget
//! worker = one thread), so repeated calls only pay for the parse, not the
//! query compile. Unknown languages fall back to a single `"default"` run
//! per line, so callers render every language uniformly (just uncolored).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;

use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

/// Map a tree-sitter capture name to a stable kind string. Kept in sync
/// with the editor's `HighlightKind` names so a widget can share a palette.
fn kind_from_capture(name: &str) -> &'static str {
    if name.starts_with("comment") {
        "comment"
    } else if name.starts_with("string") {
        "string"
    } else if name.starts_with("escape") {
        "escape"
    } else if name.starts_with("keyword") {
        "keyword"
    } else if name.starts_with("function") {
        "function"
    } else if name.starts_with("constructor") {
        "constructor"
    } else if name.starts_with("type") {
        "type"
    } else if name.starts_with("attribute") {
        "attribute"
    } else if name.starts_with("constant") {
        "constant"
    } else if name.starts_with("number") {
        // JSON/most grammars tag numerals `@number`; reuse the constant color.
        "constant"
    } else if name.starts_with("operator") {
        "operator"
    } else if name.starts_with("punctuation") {
        "punctuation"
    } else if name.starts_with("property") {
        "property"
    } else if name.starts_with("label") {
        "label"
    } else if name.starts_with("variable") {
        "variable"
    } else {
        "default"
    }
}

struct LangState {
    parser: Parser,
    query: Query,
}

thread_local! {
    /// Per-grammar parser+query, built lazily. `None` marks a grammar that
    /// failed to load so we don't retry it on every call.
    static LANGS: RefCell<HashMap<&'static str, Option<LangState>>> =
        RefCell::new(HashMap::new());
}

/// Resolve a language id or file extension to a canonical grammar key.
/// Add grammars by adding a crate dep + an arm here and in [`make_lang`].
fn canon_lang(lang: &str) -> Option<&'static str> {
    match lang.trim().trim_start_matches('.').to_ascii_lowercase().as_str() {
        "rust" | "rs" => Some("rust"),
        "json" => Some("json"),
        _ => None,
    }
}

fn make_lang(key: &str) -> Option<LangState> {
    match key {
        "rust" => {
            let mut parser = Parser::new();
            parser.set_language(&tree_sitter_rust::LANGUAGE.into()).ok()?;
            let query = Query::new(
                &tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY,
            )
            .ok()?;
            Some(LangState { parser, query })
        }
        "json" => {
            let mut parser = Parser::new();
            parser.set_language(&tree_sitter_json::LANGUAGE.into()).ok()?;
            let query = Query::new(
                &tree_sitter_json::LANGUAGE.into(),
                tree_sitter_json::HIGHLIGHTS_QUERY,
            )
            .ok()?;
            Some(LangState { parser, query })
        }
        _ => None,
    }
}

/// Highlight `code`, returning one entry per line (split on `\n`); each line
/// is a list of `(text, kind)` runs covering it left-to-right. Unknown
/// language → each line is a single `(line, "default")` run.
pub fn highlight_lines(code: &str, lang: &str) -> Vec<Vec<(String, String)>> {
    match canon_lang(lang).and_then(|key| compute_kinds(code, key)) {
        Some(kinds) => rle_lines(code, &kinds),
        None => code
            .split('\n')
            .map(|l| vec![(l.to_string(), "default".to_string())])
            .collect(),
    }
}

/// Per-byte kind buffer for the whole `code` (later captures win), or `None`
/// if the grammar is unavailable / the parse bails.
fn compute_kinds(code: &str, key: &'static str) -> Option<Vec<&'static str>> {
    LANGS.with(|cell| {
        let mut map = cell.borrow_mut();
        let entry = map.entry(key).or_insert_with(|| make_lang(key));
        let lang = entry.as_mut()?;
        let tree = lang.parser.parse(code, None)?;
        let names = lang.query.capture_names();
        let mut spans: Vec<(Range<usize>, &'static str)> = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&lang.query, tree.root_node(), code.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let kind = kind_from_capture(names[cap.index as usize]);
                if kind == "default" {
                    continue;
                }
                spans.push((cap.node.byte_range(), kind));
            }
        }
        // Stable by start; paint in order so later/longer captures overwrite
        // earlier coarse ones (the tree-sitter-rust highlights.scm convention).
        spans.sort_by_key(|(r, _)| r.start);
        let mut kinds = vec!["default"; code.len()];
        for (range, kind) in spans {
            let lo = range.start.min(code.len());
            let hi = range.end.min(code.len());
            for slot in &mut kinds[lo..hi] {
                *slot = kind;
            }
        }
        Some(kinds)
    })
}

/// Split the per-byte kinds into lines (on `\n`), RLE-encoding each line into
/// `(text, kind)` runs. Produces exactly `code.split('\n').count()` entries.
fn rle_lines(code: &str, kinds: &[&'static str]) -> Vec<Vec<(String, String)>> {
    let bytes = code.as_bytes();
    let n = code.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    loop {
        let mut end = start;
        while end < n && bytes[end] != b'\n' {
            end += 1;
        }
        out.push(rle_run(&code[start..end], &kinds[start..end]));
        if end >= n {
            break;
        }
        start = end + 1;
    }
    out
}

/// Run-length-encode one line's `(byte → kind)` into `(text, kind)` runs,
/// extending each run to a UTF-8 char boundary so multi-byte chars aren't
/// split between runs.
fn rle_run(text: &str, kinds: &[&'static str]) -> Vec<(String, String)> {
    let len = text.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < len {
        let kind = kinds[i];
        let mut j = i + 1;
        while j < len && kinds[j] == kind {
            j += 1;
        }
        while j < len && !text.is_char_boundary(j) {
            j += 1;
        }
        out.push((text[i..j].to_string(), kind.to_string()));
        i = j;
    }
    out
}
