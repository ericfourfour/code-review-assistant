//! Syntax colour from the same tree-sitter grammars everything else semantic
//! goes through.
//!
//! Every excerpt this app shows — the context pane, the original, the editable
//! final text, an evidence window — is code someone is being asked to judge,
//! and unhighlighted code is harder to read than it needs to be. The colours
//! come from each grammar's own bundled `highlights.scm`, so there is no
//! second, hand-rolled idea of what a keyword or a string is: the parser that
//! decides where a definition begins also decides what is a string literal.
//!
//! A language with no bundled grammar simply comes back uncoloured, the same
//! graceful degradation [`crate::scopes`] makes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use egui::Color32;
use tree_sitter::{Query, QueryCursor, StreamingIterator};

const KEYWORD: Color32 = Color32::from_rgb(255, 123, 114);
const FUNCTION: Color32 = Color32::from_rgb(210, 168, 255);
const TYPE: Color32 = Color32::from_rgb(255, 166, 87);
const STRING: Color32 = Color32::from_rgb(143, 197, 255);
const NUMBER: Color32 = Color32::from_rgb(121, 192, 255);
const COMMENT: Color32 = Color32::from_rgb(126, 140, 155);
const CONSTANT: Color32 = Color32::from_rgb(121, 192, 255);
const PROPERTY: Color32 = Color32::from_rgb(126, 231, 135);
const PUNCT: Color32 = Color32::from_rgb(150, 162, 175);
const ATTRIBUTE: Color32 = Color32::from_rgb(255, 166, 87);
const VARIABLE: Color32 = Color32::from_rgb(201, 209, 217);

/// A run of bytes within one line that share a colour.
pub type Run = (usize, usize, Color32);

/// Colour runs for a whole excerpt, one entry per line.
type Lines = Rc<Vec<Vec<Run>>>;

/// The colour a capture name earns, or `None` to leave the text at whatever
/// base colour the caller chose. Dotted names fall back to their head, so a
/// grammar that says `@function.method` is coloured even though only
/// `function` is listed here.
fn color_for(capture: &str) -> Option<Color32> {
    Some(match capture.split('.').next().unwrap_or(capture) {
        "keyword" => KEYWORD,
        "function" | "constructor" => FUNCTION,
        "type" => TYPE,
        "string" | "escape" | "character" => STRING,
        "number" | "float" => NUMBER,
        "comment" => COMMENT,
        "constant" => CONSTANT,
        "property" | "field" | "tag" => PROPERTY,
        "attribute" | "label" | "annotation" => ATTRIBUTE,
        "operator" | "punctuation" => PUNCT,
        "variable" => VARIABLE,
        _ => return None,
    })
}

thread_local! {
    /// Compiled queries live as long as the process: compiling one costs far
    /// more than running it, and there are ten of them at most.
    static QUERIES: RefCell<HashMap<&'static str, Option<Rc<Query>>>> =
        RefCell::new(HashMap::new());
    /// Highlighting results, keyed by language and content. The UI redraws
    /// many times a second over text that almost never changes; without this
    /// every frame would reparse.
    static SPANS: RefCell<HashMap<(&'static str, u64), Lines>> =
        RefCell::new(HashMap::new());
}

/// Above this many cached excerpts the cache is dropped whole. A review
/// touches a bounded working set, so a plain clear is enough — and cheaper
/// than tracking use order for something purely cosmetic.
const MAX_CACHED: usize = 256;

/// Colour runs for `lines`, which are parsed **together** as one snippet of
/// whatever language `path` implies. The result has one entry per input line,
/// each holding byte ranges within that line; a line the grammar claims
/// nothing in comes back empty, as does every line when the language has no
/// bundled grammar.
///
/// Passing the lines together rather than one at a time is the point: a string
/// literal or a block comment that runs across lines is only visible to a
/// parser that sees them as one text.
pub fn line_spans(path: &str, lines: &[&str]) -> Lines {
    let Some((language, query_src)) = crate::scopes::highlight_grammar(path) else {
        return Rc::new(vec![Vec::new(); lines.len()]);
    };
    let key = (query_src, content_hash(lines));
    if let Some(hit) = SPANS.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let Some(query) = query_for(&language, query_src) else {
        return Rc::new(vec![Vec::new(); lines.len()]);
    };
    let computed = Rc::new(compute(&language, &query, lines));
    SPANS.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.len() >= MAX_CACHED {
            cache.clear();
        }
        cache.insert(key, computed.clone());
    });
    computed
}

fn content_hash(lines: &[&str]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    lines.len().hash(&mut hasher);
    for line in lines {
        line.hash(&mut hasher);
    }
    hasher.finish()
}

/// A query that fails to compile against its own grammar costs that language
/// its colour, never a panic — and the `None` is cached, so the failure costs
/// one compile rather than one per frame.
fn query_for(language: &tree_sitter::Language, src: &'static str) -> Option<Rc<Query>> {
    QUERIES.with(|c| {
        c.borrow_mut()
            .entry(src)
            .or_insert_with(|| Query::new(language, src).ok().map(Rc::new))
            .clone()
    })
}

fn compute(language: &tree_sitter::Language, query: &Query, lines: &[&str]) -> Vec<Vec<Run>> {
    let empty = || vec![Vec::new(); lines.len()];
    let source = lines.join("\n");
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(language).is_err() {
        return empty();
    }
    let Some(tree) = parser.parse(&source, None) else {
        return empty();
    };

    // Collect every capture, then paint widest-first so a nested capture (the
    // name inside a call, the escape inside a string) is drawn over the one
    // enclosing it. Ties go to the earlier pattern, which is the convention
    // every `highlights.scm` is written to.
    let names = query.capture_names();
    let mut caps: Vec<(usize, usize, usize, Color32)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut hits = cursor.captures(query, tree.root_node(), source.as_bytes());
    while let Some((m, idx)) = hits.next() {
        let cap = m.captures[*idx];
        let Some(name) = names.get(cap.index as usize) else {
            continue;
        };
        let Some(color) = color_for(name) else {
            continue;
        };
        let range = cap.node.byte_range();
        if range.end > range.start && range.end <= source.len() {
            caps.push((range.start, range.end, m.pattern_index, color));
        }
    }
    caps.sort_by_key(|(s, e, pattern, _)| (std::cmp::Reverse(e - s), std::cmp::Reverse(*pattern)));
    let mut paint: Vec<Option<Color32>> = vec![None; source.len()];
    for (start, end, _, color) in caps {
        for color_entry in &mut paint[start..end] {
            *color_entry = Some(color);
        }
    }

    // Cut the painted bytes back into per-line runs. Every offset came from a
    // node boundary, so every run edge is a character boundary.
    let mut out = Vec::with_capacity(lines.len());
    let mut offset = 0usize;
    for line in lines {
        let mut runs: Vec<Run> = Vec::new();
        let mut i = 0usize;
        while i < line.len() {
            let color = paint[offset + i];
            let start = i;
            while i < line.len() && paint[offset + i] == color {
                i += 1;
            }
            if let Some(color) = color {
                runs.push((start, i, color));
            }
        }
        out.push(runs);
        offset += line.len() + 1; // the '\n' the join put back
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors(path: &str, lines: &[&str]) -> Vec<Vec<(String, Color32)>> {
        let spans = line_spans(path, lines);
        spans
            .iter()
            .zip(lines)
            .map(|(runs, line)| {
                runs.iter()
                    .map(|(s, e, c)| (line[*s..*e].to_string(), *c))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn rust_keywords_strings_and_comments_get_their_own_colours() {
        let out = colors("a.rs", &["// note", "fn f() -> u32 {", "    \"text\"", "}"]);
        assert!(
            out[0]
                .iter()
                .any(|(t, c)| t.contains("note") && *c == COMMENT),
            "{:?}",
            out[0]
        );
        assert!(
            out[1].iter().any(|(t, c)| t == "fn" && *c == KEYWORD),
            "{:?}",
            out[1]
        );
        assert!(
            out[2]
                .iter()
                .any(|(t, c)| t.contains("text") && *c == STRING),
            "{:?}",
            out[2]
        );
    }

    #[test]
    fn a_string_holding_code_is_coloured_as_a_string() {
        // The same trap that killed the hand-rolled scope scanner: `fn` and
        // braces inside a literal are string, not keyword and punctuation.
        let out = colors("a.rs", &["const F: &str = \"fn main() {}\";"]);
        let fake = out[0].iter().find(|(t, _)| t.contains("fn main"));
        assert!(fake.is_some_and(|(_, c)| *c == STRING), "{:?}", out[0]);
    }

    #[test]
    fn runs_stay_inside_their_own_line() {
        // A block comment spanning lines still yields one run per line, each
        // addressable with that line's own byte offsets.
        let lines = ["/* one", "   two */", "let x = 1;"];
        let spans = line_spans("a.rs", &lines);
        assert_eq!(spans.len(), 3);
        for (runs, line) in spans.iter().zip(lines) {
            for (s, e, _) in runs {
                assert!(*e <= line.len(), "run {s}..{e} escapes {line:?}");
                assert!(line.is_char_boundary(*s) && line.is_char_boundary(*e));
            }
        }
        assert!(
            !spans[1].is_empty(),
            "the middle of a block comment is still comment"
        );
    }

    #[test]
    fn multibyte_source_keeps_char_boundaries() {
        let lines = ["let s = \"héllo — ok\";", "// naïve"];
        let spans = line_spans("a.rs", &lines);
        for (runs, line) in spans.iter().zip(lines) {
            for (s, e, _) in runs {
                assert!(
                    line.is_char_boundary(*s) && line.is_char_boundary(*e),
                    "{s}..{e}"
                );
            }
        }
    }

    #[test]
    fn every_bundled_grammar_compiles_its_own_query() {
        // A query that does not compile against its grammar silently costs
        // that language all of its colour, which is easy not to notice.
        for path in [
            "a.rs", "a.py", "a.js", "a.jsx", "a.ts", "a.tsx", "a.go", "a.java", "a.c", "a.h",
            "a.cpp", "a.cs", "a.php",
        ] {
            let (language, src) = crate::scopes::highlight_grammar(path).expect(path);
            assert!(
                query_for(&language, src).is_some(),
                "{path} has no usable highlight query"
            );
        }
    }

    #[test]
    fn a_language_with_no_grammar_is_left_alone() {
        let spans = line_spans("notes.md", &["# heading", "text"]);
        assert_eq!(spans.len(), 2);
        assert!(spans.iter().all(|r| r.is_empty()));
    }

    #[test]
    fn a_fragment_that_does_not_parse_still_colours_what_it_can() {
        // Excerpts are fragments by nature: a method shown without its impl
        // block, a hunk cut mid-expression.
        let out = colors("a.rs", &["    self.count += 1;", "    // done", "}"]);
        assert!(out[1].iter().any(|(_, c)| *c == COMMENT), "{:?}", out[1]);
    }
}
