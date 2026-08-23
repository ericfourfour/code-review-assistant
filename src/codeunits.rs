//! Extract reviewable *code* units from diff hunks — the counterpart of
//! [`crate::comments`] for everything that is not a comment.
//!
//! A code unit is a cluster of changed lines the branch introduced (added
//! code, or the place where code was removed). The unit's editable region is
//! kept tight — just the cluster — while its *context* is as wide as can be
//! justified: the whole enclosing function or class when the language is
//! parsable enough to find it (a "semantic" unit), the surrounding hunk when
//! it is not. Hunk units need nothing but the diff, so unparsable or unknown
//! files still get reviewed; semantic units are preferred when possible so
//! the models and the human judge the change against the definition it
//! actually lives in.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::comments::LangSpec;
use crate::diffparse::DiffFile;

/// Widest excerpt a unit may show. An enclosing scope larger than this is
/// judged from the hunk instead — past a point a wall of code is worse
/// context than a window onto the change.
pub const MAX_SCOPE_LINES: usize = 240;

/// Changed lines this close together are reviewed as one unit. Wider gaps
/// split, so two unrelated edits that happen to share a hunk get separate
/// verdicts.
const CLUSTER_GAP: u32 = 10;

/// An editable region longer than this is split into windows — a brand-new
/// thousand-line file is not one reviewable thought (dogfooding produced a
/// single 993-line unit before this existed). Splits prefer blank lines so
/// the windows follow the file's own paragraphing.
const MAX_REGION_LINES: u32 = 120;

/// How many lines a multi-line signature / attribute stack may extend a
/// scope's start upward.
const MAX_SIGNATURE_LINES: usize = 12;

#[derive(Clone, Serialize, Deserialize)]
pub struct CodeUnit {
    pub file: String,
    pub lang: String,
    /// 1-based line numbers (new side) of the editable region — the cluster
    /// of changed lines, not the whole excerpt shown in `context`.
    pub start_line: u32,
    pub end_line: u32,
    /// The exact file lines of the region.
    pub raw_lines: Vec<String>,
    /// New-side line numbers within the region that the branch added. Empty
    /// for a unit that only anchors a removal.
    pub changed_lines: Vec<u32>,
    /// Header of the enclosing definition when one was found (semantic unit);
    /// `None` means the context is the hunk.
    pub scope: Option<String>,
    /// Rendered excerpt: `>` marks region lines, `+` other added lines, `-`
    /// removed lines, interleaved where they were removed from.
    pub context: String,
    pub hunk_header: String,
}

pub fn extract(repo: &str, files: &[DiffFile], context_lines: usize) -> Vec<(String, Vec<CodeUnit>)> {
    let mut out = Vec::new();
    for f in files {
        let units = units_in_file(repo, f, context_lines);
        if !units.is_empty() {
            out.push((f.path.clone(), units));
        }
    }
    out
}

/// Language label for a file the comment extractor has no spec for. Unknown
/// files are still code someone changed, so they still get hunk units.
fn lang_fallback(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit('.').next() {
        Some(ext) if !ext.is_empty() && ext != name => ext.to_string(),
        _ => "text".into(),
    }
}

fn units_in_file(repo: &str, f: &DiffFile, context_lines: usize) -> Vec<CodeUnit> {
    let spec = crate::comments::lang_for(&f.path);
    let lang = spec.as_ref().map(|s| s.name.to_string()).unwrap_or_else(|| lang_fallback(&f.path));

    // File-level view of the diff: every new-side line, which of them were
    // added, and what was removed after which line.
    let mut new_text: BTreeMap<u32, String> = BTreeMap::new();
    let mut added: BTreeSet<u32> = BTreeSet::new();
    let mut removals: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for hunk in &f.hunks {
        let mut last_new = hunk.new_start.saturating_sub(1);
        for l in &hunk.lines {
            if l.origin == '-' {
                removals.entry(last_new).or_default().push(l.text.clone());
            } else if let Some(no) = l.new_lineno {
                new_text.insert(no, l.text.clone());
                if l.origin == '+' {
                    added.insert(no);
                }
                last_new = no;
            }
        }
    }

    let disk: Option<Vec<String>> = std::fs::read_to_string(Path::new(repo).join(&f.path))
        .ok()
        .map(|c| c.lines().map(|s| s.to_string()).collect());

    let mut units = Vec::new();
    for hunk in &f.hunks {
        let span: Vec<u32> = hunk.lines.iter().filter_map(|l| l.new_lineno).collect();
        let (Some(&first_new), Some(&last_new)) = (span.first(), span.last()) else { continue };

        // Positions worth reviewing: added lines that are code, plus an
        // anchor for every run of removed code. A removal directly replaced
        // by added code needs no anchor of its own — the replacement's lines
        // already put the change under review. Comment-only and blank-only
        // changes belong to the comment flow, not this one.
        let comment_added = comment_flags(spec.as_ref(), hunk);
        let is_added_code = |l: &crate::diffparse::HunkLine| {
            l.origin == '+'
                && !l.text.trim().is_empty()
                && l.new_lineno.is_some_and(|no| !comment_added.contains(&no))
        };
        let mut positions: BTreeSet<u32> = BTreeSet::new();
        let mut cursor = hunk.new_start.saturating_sub(1);
        let mut idx = 0;
        while idx < hunk.lines.len() {
            let l = &hunk.lines[idx];
            if let Some(no) = l.new_lineno {
                if is_added_code(l) {
                    positions.insert(no);
                }
                cursor = no;
                idx += 1;
                continue;
            }
            // A run of removed lines.
            let mut has_code = false;
            while idx < hunk.lines.len() && hunk.lines[idx].new_lineno.is_none() {
                if removed_is_code(spec.as_ref(), &hunk.lines[idx].text) {
                    has_code = true;
                }
                idx += 1;
            }
            let covered = hunk.lines.get(idx).is_some_and(&is_added_code);
            if has_code && !covered {
                let anchor = if cursor >= first_new {
                    cursor
                } else {
                    hunk.lines.get(idx).and_then(|n| n.new_lineno).unwrap_or(first_new)
                };
                positions.insert(anchor.clamp(first_new, last_new));
            }
        }
        if positions.is_empty() {
            continue;
        }

        let windows: Vec<(u32, u32)> = clusters(&positions, CLUSTER_GAP)
            .into_iter()
            .flat_map(|c| split_region(c, &new_text))
            .collect();
        for (rs, re) in windows {
            let raw_lines: Vec<String> = match (rs..=re).map(|no| new_text.get(&no).cloned()).collect() {
                Some(lines) => lines,
                None => continue, // hole in the diff — should not happen
            };
            let changed_lines: Vec<u32> = added.range(rs..=re).copied().collect();
            let (scope, context) = context_for(
                disk.as_deref(),
                spec.as_ref(),
                &lang,
                (rs, re),
                (first_new, last_new),
                &new_text,
                &added,
                &removals,
                context_lines,
            );
            units.push(CodeUnit {
                file: f.path.clone(),
                lang: lang.clone(),
                start_line: rs,
                end_line: re,
                raw_lines,
                changed_lines,
                scope,
                context,
                hunk_header: hunk.header.clone(),
            });
        }
    }
    units
}

/// New-side line numbers in this hunk that are (part of) whole-line comments,
/// tracking block comments across lines so their middles count too.
fn comment_flags(spec: Option<&LangSpec>, hunk: &crate::diffparse::Hunk) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    let Some(spec) = spec else { return out };
    let mut in_block = false;
    for l in hunk.lines.iter().filter(|l| l.origin != '-') {
        let Some(no) = l.new_lineno else { continue };
        let t = l.text.trim_start();
        if in_block {
            out.insert(no);
            if let Some((_, close)) = spec.block {
                if t.contains(close) {
                    in_block = false;
                }
            }
            continue;
        }
        if spec.line_prefixes.iter().any(|p| t.starts_with(p)) {
            out.insert(no);
        } else if let Some((open, close)) = spec.block {
            if let Some(rest) = t.strip_prefix(open) {
                out.insert(no);
                if !rest.contains(close) {
                    in_block = true;
                }
            }
        }
    }
    out
}

/// Whether a removed line was code. Removing a comment is the comment flow's
/// concern (or nobody's); removing code deserves a code verdict.
fn removed_is_code(spec: Option<&LangSpec>, text: &str) -> bool {
    let t = text.trim_start();
    if t.is_empty() {
        return false;
    }
    let Some(spec) = spec else { return true };
    if spec.line_prefixes.iter().any(|p| t.starts_with(p)) {
        return false;
    }
    if let Some((open, _)) = spec.block {
        // The opener, or the `* ...` / `*/` middle and end of a conventional
        // block comment. `*` alone would also match `*ptr = x`, so require a
        // space or slash after it.
        if t.starts_with(open) || t.starts_with("* ") || t.starts_with("*/") || t == "*" {
            return false;
        }
    }
    true
}

/// Chop an oversized region into consecutive windows of at most
/// [`MAX_REGION_LINES`], cutting at a blank line in the back half of each
/// window when one exists.
fn split_region(region: (u32, u32), new_text: &BTreeMap<u32, String>) -> Vec<(u32, u32)> {
    let (mut start, end) = region;
    let mut out = Vec::new();
    while end - start + 1 > MAX_REGION_LINES {
        let hard = start + MAX_REGION_LINES - 1;
        let cut = (start + MAX_REGION_LINES / 2..=hard)
            .rev()
            .find(|no| new_text.get(no).is_some_and(|t| t.trim().is_empty()))
            .unwrap_or(hard);
        out.push((start, cut));
        start = cut + 1;
    }
    out.push((start, end));
    out
}

fn clusters(positions: &BTreeSet<u32>, gap: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut cur: Option<(u32, u32)> = None;
    for &p in positions {
        cur = match cur {
            Some((s, e)) if p <= e.saturating_add(gap) => Some((s, p)),
            Some(done) => {
                out.push(done);
                Some((p, p))
            }
            None => Some((p, p)),
        };
    }
    if let Some(done) = cur {
        out.push(done);
    }
    out
}

/// The unit's excerpt: the enclosing definition when it can be found on disk
/// and is not enormous, the hunk window otherwise.
#[allow(clippy::too_many_arguments)]
fn context_for(
    disk: Option<&[String]>,
    spec: Option<&LangSpec>,
    lang: &str,
    region: (u32, u32),
    hunk_span: (u32, u32),
    new_text: &BTreeMap<u32, String>,
    added: &BTreeSet<u32>,
    removals: &BTreeMap<u32, Vec<String>>,
    context_lines: usize,
) -> (Option<String>, String) {
    let (rs, re) = region;
    if let Some(disk) = disk {
        // The diff and the working tree must agree before disk lines can be
        // shown as "the new side" — an already-dirty checkout must not put
        // words in the branch's mouth.
        let matches = (rs..=re).all(|no| {
            disk.get(no as usize - 1).map(|s| s.as_str()) == new_text.get(&no).map(|s| s.as_str())
        });
        if matches {
            if let Some(scope) = scope_for(disk, spec, lang, rs as usize - 1) {
                let start0 = scope.start0.min(rs as usize - 1);
                let end0 = scope.end0.max(re as usize - 1).min(disk.len().saturating_sub(1));
                if end0 - start0 < MAX_SCOPE_LINES {
                    let numbered: Vec<(u32, &str)> = (start0..=end0)
                        .map(|i| (i as u32 + 1, disk[i].as_str()))
                        .collect();
                    return (Some(scope.header), render(&numbered, region, added, removals));
                }
            }
        }
    }
    // Hunk window: the cluster with breathing room, clamped to the hunk.
    let ws = rs.saturating_sub(context_lines as u32).max(hunk_span.0);
    let we = re.saturating_add(context_lines as u32).min(hunk_span.1);
    let numbered: Vec<(u32, &str)> = (ws..=we)
        .filter_map(|no| new_text.get(&no).map(|t| (no, t.as_str())))
        .collect();
    (None, render(&numbered, region, added, removals))
}

/// Lay out numbered lines with markers, interleaving removed lines where they
/// were removed from. `>` region under review, `+` other added lines, `-`
/// removed, ` ` untouched.
fn render(
    numbered: &[(u32, &str)],
    region: (u32, u32),
    added: &BTreeSet<u32>,
    removals: &BTreeMap<u32, Vec<String>>,
) -> String {
    let mut out = String::new();
    let removed = |out: &mut String, key: u32| {
        if let Some(lines) = removals.get(&key) {
            for r in lines {
                out.push_str(&format!("-     | {r}\n"));
            }
        }
    };
    if let Some(&(first, _)) = numbered.first() {
        removed(&mut out, first.saturating_sub(1));
    }
    for &(no, text) in numbered {
        let marker = if no >= region.0 && no <= region.1 {
            '>'
        } else if added.contains(&no) {
            '+'
        } else {
            ' '
        };
        out.push_str(&format!("{marker}{no:>5}| {text}\n"));
        removed(&mut out, no);
    }
    out
}

// ---------------------------------------------------------------------------
// Scope detection: which definition encloses a line. Heuristic on purpose —
// no parser, so it works on any file that mostly follows its language's
// shape, and simply returns None (hunk fallback) on anything it cannot read.

struct Scope {
    start0: usize,
    end0: usize,
    header: String,
}

fn scope_for(lines: &[String], spec: Option<&LangSpec>, lang: &str, target0: usize) -> Option<Scope> {
    if target0 >= lines.len() {
        return None;
    }
    match lang {
        "Python" => indent_scope(lines, target0),
        "Rust" | "C/C++" | "JavaScript" | "TypeScript" | "JVM" | "Go" | "Swift" | "C#" | "PHP" => {
            brace_scope(lines, spec, target0)
        }
        _ => None,
    }
}

/// Brace languages: the innermost `{...}` block containing the target whose
/// header reads like a definition (or that opens at the top level, which
/// covers C functions and other keyword-less forms).
fn brace_scope(lines: &[String], spec: Option<&LangSpec>, target0: usize) -> Option<Scope> {
    let stripped = strip_noncode(lines, spec);
    let mut stack: Vec<usize> = Vec::new();
    let mut enclosing: Vec<(usize, usize, usize)> = Vec::new(); // (open, close, depth)
    for (i, code) in stripped.iter().enumerate() {
        for ch in code.chars() {
            match ch {
                '{' => stack.push(i),
                '}' => {
                    if let Some(open) = stack.pop() {
                        if open <= target0 && i >= target0 {
                            // Blocks close inner-first, so this vec is already
                            // ordered innermost to outermost.
                            enclosing.push((open, i, stack.len()));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    for (open, close, depth) in enclosing {
        // A `{` on its own line belongs to the signature above it.
        let mut header_line = open;
        while header_line > 0 && stripped[header_line][..].trim_start().starts_with('{') {
            header_line -= 1;
            if !lines[header_line].trim().is_empty() {
                break;
            }
        }
        let start0 = extend_signature(lines, spec, header_line);
        let label_line = (start0..=header_line)
            .find(|&i| {
                let t = lines[i].trim();
                !t.is_empty() && !is_attached_line(t, spec)
            })
            .unwrap_or(header_line);
        let label = lines[label_line].trim();
        if depth == 0 || is_definition_header(label) {
            return Some(Scope { start0, end0: close, header: scope_label(label) });
        }
    }
    None
}

/// Multi-line signatures, attributes/decorators, and directly attached doc
/// comments belong to the definition — pull the scope's start up over them.
fn extend_signature(lines: &[String], spec: Option<&LangSpec>, header_line: usize) -> usize {
    let mut start = header_line;
    while start > 0 && header_line - start < MAX_SIGNATURE_LINES {
        let prev = lines[start - 1].trim();
        if prev.is_empty() {
            break;
        }
        let continues = prev.ends_with(',') || prev.ends_with('(');
        // A bare `static int` / `unsigned long` line above a C-style function
        // name is part of the signature too.
        let type_line = prev
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ' ' || c == '\t' || c == '*');
        if continues || type_line || is_attached_line(prev, spec) {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

/// Attribute, annotation, or comment lines that ride along with a definition.
fn is_attached_line(trimmed: &str, spec: Option<&LangSpec>) -> bool {
    if trimmed.starts_with("#[") || trimmed.starts_with('@') {
        return true;
    }
    spec.map(|s| s.line_prefixes.iter().any(|p| trimmed.starts_with(p))).unwrap_or(false)
}

/// Does this line read like the head of a definition rather than control
/// flow? Modifier keywords are stripped first, so `pub async fn`, `public
/// static void`, and friends all resolve to their core.
fn is_definition_header(line: &str) -> bool {
    let mut t = line.trim();
    loop {
        let before = t;
        for m in [
            "pub(crate)", "pub(super)", "pub(in", "pub", "export", "default", "static", "public",
            "private", "protected", "internal", "abstract", "final", "sealed", "override",
            "virtual", "async", "unsafe", "extern", "inline", "constexpr",
        ] {
            if let Some(rest) = t.strip_prefix(m) {
                if rest.starts_with(' ') || rest.starts_with(')') {
                    t = rest.trim_start_matches(')').trim_start();
                    break;
                }
            }
        }
        if t == before {
            break;
        }
    }
    let first: String = t
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if matches!(
        first.as_str(),
        "if" | "else" | "while" | "for" | "switch" | "match" | "loop" | "do" | "catch" | "try"
            | "return" | "unless" | "select" | "defer" | "go"
    ) {
        return false;
    }
    if matches!(
        first.as_str(),
        "fn" | "def" | "func" | "function" | "class" | "impl" | "trait" | "struct" | "enum"
            | "union" | "interface" | "record" | "object" | "namespace" | "mod" | "constructor"
            | "init"
    ) {
        return true;
    }
    // Keyword-less function heads: `int main(void) {`, `void Foo::bar() const {`
    t.contains('(') && (t.ends_with('{') || t.ends_with(')') || t.ends_with("=> {"))
}

fn scope_label(header: &str) -> String {
    let cleaned = header.trim_end_matches('{').trim_end();
    crate::app::truncate(cleaned, 60)
}

/// Per-line code with strings and comments blanked out, so brace counting
/// sees only structural braces. Heuristic: raw strings and multi-line string
/// literals are not tracked — a file that defeats this simply gets a hunk
/// unit instead of a semantic one.
fn strip_noncode(lines: &[String], spec: Option<&LangSpec>) -> Vec<String> {
    let block = spec.and_then(|s| s.block);
    let no_prefixes: Vec<&str> = Vec::new();
    let prefixes: Vec<&str> =
        spec.map(|s| s.line_prefixes.to_vec()).unwrap_or(no_prefixes);
    let mut in_block = false;
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let s = line.as_str();
        let mut code = String::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            if in_block {
                if let Some((_, close)) = block {
                    if s[i..].starts_with(close) {
                        in_block = false;
                        i += close.len();
                        continue;
                    }
                }
                i += s[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                continue;
            }
            if prefixes.iter().any(|p| s[i..].starts_with(p)) {
                break;
            }
            if let Some((open, close)) = block {
                if s[i..].starts_with(open) {
                    in_block = true;
                    i += open.len();
                    // `/* ... */` on one line closes immediately in the loop.
                    let _ = close;
                    continue;
                }
            }
            let c = s[i..].chars().next().unwrap();
            match c {
                '"' | '`' => {
                    i += c.len_utf8();
                    i += skip_string(&s[i..], c);
                }
                '\'' => {
                    // A char literal (possibly escaped, `'{'` included) is
                    // skipped; a lone quote — a lifetime, an apostrophe — is
                    // left alone rather than swallowing the rest of the line.
                    i += c.len_utf8();
                    match char_literal_len(&s[i..]) {
                        Some(n) => i += n,
                        None => code.push(' '),
                    }
                }
                '{' | '}' => {
                    code.push(c);
                    i += 1;
                }
                _ => {
                    code.push(if c.is_whitespace() { ' ' } else { 'x' });
                    i += c.len_utf8();
                }
            }
        }
        out.push(code);
    }
    out
}

/// Bytes to skip until the closing `quote` (or end of line), escapes honoured.
fn skip_string(rest: &str, quote: char) -> usize {
    let mut esc = false;
    let mut n = 0;
    for c in rest.chars() {
        n += c.len_utf8();
        if esc {
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == quote {
            return n;
        }
    }
    n
}

/// Length of a short `...'` char-literal body, or None if the quote does not
/// close within a few characters (then it was not a char literal at all).
fn char_literal_len(rest: &str) -> Option<usize> {
    let mut n = 0;
    for (count, c) in rest.chars().enumerate() {
        n += c.len_utf8();
        if c == '\'' && count > 0 {
            return Some(n);
        }
        // `'a'`, `'\n'`, `'\u{7f}'` at most — anything longer is a lifetime.
        if count >= 8 {
            break;
        }
    }
    // `''` (empty) or unterminated within reach.
    if rest.starts_with('\'') {
        return Some(1);
    }
    None
}

/// Python and friends: the nearest `def`/`class` line above the target with
/// less indentation than everything between them.
fn indent_scope(lines: &[String], target0: usize) -> Option<Scope> {
    let width = |s: &str| -> usize {
        s.chars()
            .take_while(|c| c.is_whitespace())
            .map(|c| if c == '\t' { 8 } else { 1 })
            .sum()
    };
    let is_def = |s: &str| {
        let t = s.trim_start();
        t.starts_with("def ") || t.starts_with("async def ") || t.starts_with("class ")
    };
    let mut t = target0;
    while t > 0 && lines[t].trim().is_empty() {
        t -= 1;
    }
    let def = if is_def(&lines[t]) {
        t
    } else {
        let mut bound = width(&lines[t]);
        let mut found = None;
        for i in (0..t).rev() {
            if lines[i].trim().is_empty() {
                continue;
            }
            let w = width(&lines[i]);
            if w < bound {
                if is_def(&lines[i]) {
                    found = Some(i);
                    break;
                }
                bound = w;
                if w == 0 {
                    break;
                }
            }
        }
        found?
    };
    let dw = width(&lines[def]);
    let mut end = def;
    for (j, line) in lines.iter().enumerate().skip(def + 1) {
        if line.trim().is_empty() {
            continue;
        }
        if width(line) <= dw {
            break;
        }
        end = j;
    }
    let mut start = def;
    while start > 0 && def - start < MAX_SIGNATURE_LINES {
        let prev = lines[start - 1].trim();
        if prev.starts_with('@') {
            start -= 1;
        } else {
            break;
        }
    }
    let header = lines[def].trim().trim_end_matches(':');
    Some(Scope { start0: start, end0: end, header: scope_label(header) })
}

/// The prompt sent to each model CLI for a code unit. Same contract as the
/// comment prompt: short, contextual, and honest about the fact that the
/// repository is there to be read.
pub fn build_prompt(u: &CodeUnit) -> String {
    let what = match &u.scope {
        Some(s) => format!("the full enclosing definition ({s}) with the change inside it"),
        None => "the diff hunk around the change".to_string(),
    };
    format!(
        "File: {} ({})\n\n{}\nReview the code change on lines {}-{}: '>' marks the lines under \
review, '+' other lines this branch added, '-' lines it removed. The excerpt shows {what}.\n\n\
You are running in the root of the repository this change lives in, and the path above is \
relative to it. Read whatever you need before judging: callers, callees, the tests, the code \
this replaces. Judge correctness first — bugs, missed edge cases, broken invariants — then \
fit: duplication of something that already exists, or style that fights the surrounding \
code.\n\n\
Verdicts: \"approve\" — sound as written. \"revise\" — you can write it better: put a full \
replacement for exactly lines {}-{} in \"replacement\". \"flag\" — something is wrong that \
rewriting these lines alone cannot fix: say what and where in \"justification\". \"delete\" — \
these lines should not exist at all.\n\n{}",
        u.file,
        u.lang,
        u.context,
        u.start_line,
        u.end_line,
        u.start_line,
        u.end_line,
        crate::models::answer_schema(true)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;

    fn write(dir: &TempDir, rel: &str, content: &str) {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn extract_one(repo: &str, diff: &str) -> Vec<(String, Vec<CodeUnit>)> {
        extract(repo, &crate::diffparse::parse(diff), 12)
    }

    const RS_FILE: &str = "\
use std::io;

/// Reads one line.
fn read_line() -> String {
    let mut s = String::new();
    io::stdin().read_line(&mut s).unwrap();
    s
}

fn main() {
    // Say hello politely
    let name = read_line();
    println!(\"hello {name}\");
}
";

    const RS_DIFF: &str = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,4 +10,5 @@
 fn main() {
+    // Say hello politely
+    let name = read_line();
-    println!(\"hello\");
+    println!(\"hello {name}\");
 }
";

    #[test]
    fn a_changed_function_becomes_one_semantic_unit() {
        let dir = TempDir::new("semantic");
        write(&dir, "src/main.rs", RS_FILE);
        let out = extract_one(&dir.path().to_string_lossy(), RS_DIFF);
        assert_eq!(out.len(), 1);
        let units = &out[0].1;
        assert_eq!(units.len(), 1, "both added lines share a scope and a cluster");
        let u = &units[0];
        assert_eq!(u.scope.as_deref(), Some("fn main()"));
        // The region is the changed cluster (code lines only start it, but the
        // comment between them rides along inside the cluster).
        assert_eq!((u.start_line, u.end_line), (12, 13));
        assert_eq!(u.changed_lines, vec![12, 13]);
        // The context is the whole function, with the doc line of the region
        // marked and the removal shown where it happened.
        assert!(u.context.contains("fn main() {"), "{}", u.context);
        assert!(u.context.contains(">   12| "), "{}", u.context);
        assert!(u.context.contains("-     | "), "{}", u.context);
        assert!(u.context.contains("println!(\"hello\");"), "{}", u.context);
        // The comment line above the cluster is added but outside the region.
        assert!(u.context.contains("+   11| "), "{}", u.context);
    }

    #[test]
    fn a_comment_only_hunk_yields_no_code_unit() {
        let dir = TempDir::new("commentonly");
        write(&dir, "src/main.rs", RS_FILE);
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,3 +10,4 @@
 fn main() {
+    // Say hello politely
     let name = read_line();
";
        assert!(extract_one(&dir.path().to_string_lossy(), diff).is_empty());
    }

    #[test]
    fn an_unknown_language_still_gets_a_hunk_unit() {
        let dir = TempDir::new("unknown");
        write(&dir, "config.conf", "a = 1\nb = 2\nc = 3\n");
        let diff = "\
diff --git a/config.conf b/config.conf
--- a/config.conf
+++ b/config.conf
@@ -1,2 +1,3 @@
 a = 1
+b = 2
 c = 3
";
        let out = extract_one(&dir.path().to_string_lossy(), diff);
        assert_eq!(out.len(), 1);
        let u = &out[0].1[0];
        assert!(u.scope.is_none(), "no scope detection for unknown languages");
        assert_eq!((u.start_line, u.end_line), (2, 2));
        assert_eq!(u.lang, "conf");
        assert!(u.context.contains(">    2| b = 2"), "{}", u.context);
    }

    #[test]
    fn a_pure_removal_is_anchored_and_shown() {
        let dir = TempDir::new("removal");
        write(&dir, "src/main.rs", "fn main() {\n    keep();\n}\n");
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,3 @@
 fn main() {
     keep();
-    gone();
 }
";
        let out = extract_one(&dir.path().to_string_lossy(), diff);
        assert_eq!(out.len(), 1);
        let u = &out[0].1[0];
        // Anchored to the line the removal followed; nothing was added.
        assert_eq!((u.start_line, u.end_line), (2, 2));
        assert!(u.changed_lines.is_empty());
        assert!(u.context.contains("-     | ") && u.context.contains("gone();"), "{}", u.context);
    }

    #[test]
    fn removing_only_a_comment_is_not_a_code_change() {
        let dir = TempDir::new("rmcomment");
        write(&dir, "src/main.rs", "fn main() {\n    keep();\n}\n");
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,3 @@
 fn main() {
     keep();
-    // old note
 }
";
        assert!(extract_one(&dir.path().to_string_lossy(), diff).is_empty());
    }

    #[test]
    fn distant_changes_in_one_hunk_split_into_clusters() {
        let dir = TempDir::new("clusters");
        let body: String = (1..=30).map(|i| format!("line{i}();\n")).collect();
        write(&dir, "a.js", &body);
        // Two additions 20 lines apart inside one big hunk.
        let mut diff = String::from(
            "diff --git a/a.js b/a.js\n--- a/a.js\n+++ b/a.js\n@@ -1,28 +1,30 @@\n",
        );
        for i in 1..=30 {
            let origin = if i == 3 || i == 25 { "+" } else { " " };
            diff.push_str(&format!("{origin}line{i}();\n"));
        }
        let out = extract_one(&dir.path().to_string_lossy(), &diff);
        let units = &out[0].1;
        assert_eq!(units.len(), 2, "a {CLUSTER_GAP}-line gap should split");
        assert_eq!((units[0].start_line, units[0].end_line), (3, 3));
        assert_eq!((units[1].start_line, units[1].end_line), (25, 25));
    }

    #[test]
    fn a_huge_new_file_splits_into_windows_at_blank_lines() {
        let dir = TempDir::new("windows");
        // A brand-new 300-line file: blocks of 9 lines separated by blanks.
        let mut body = String::new();
        for i in 0..300 {
            if i % 10 == 9 {
                body.push('\n');
            } else {
                body.push_str(&format!("line{i}();\n"));
            }
        }
        write(&dir, "big.js", &body);
        let mut diff = String::from("diff --git a/big.js b/big.js\n--- a/big.js\n+++ b/big.js\n@@ -0,0 +1,300 @@\n");
        for line in body.lines() {
            diff.push_str(&format!("+{line}\n"));
        }
        let out = extract_one(&dir.path().to_string_lossy(), &diff);
        let units = &out[0].1;
        assert!(units.len() >= 3, "300 added lines should not be one unit: {}", units.len());
        for (i, u) in units.iter().enumerate() {
            let len = u.end_line - u.start_line + 1;
            assert!(len <= MAX_REGION_LINES, "window {i} is {len} lines");
            assert!(!u.changed_lines.is_empty(), "window {i} reviews nothing");
            if i > 0 {
                assert_eq!(u.start_line, units[i - 1].end_line + 1, "windows must stay consecutive");
            }
            if i < units.len() - 1 {
                assert!(
                    u.raw_lines.last().is_some_and(|l| l.trim().is_empty()),
                    "window {i} should cut at a blank line: {:?}",
                    u.raw_lines.last()
                );
            }
        }
        assert_eq!(units.first().unwrap().start_line, 1);
        assert_eq!(units.last().unwrap().end_line, 299, "the last content line ends the last window");
    }

    #[test]
    fn a_dirty_checkout_falls_back_to_the_hunk() {
        let dir = TempDir::new("dirty");
        // Disk disagrees with the diff's new side: no scope, hunk context.
        write(&dir, "src/main.rs", "fn something_else() {}\n");
        let out = extract_one(&dir.path().to_string_lossy(), RS_DIFF);
        let u = &out[0].1[0];
        assert!(u.scope.is_none());
        assert!(u.context.contains(">   12| "), "{}", u.context);
    }

    #[test]
    fn an_oversized_scope_falls_back_to_the_hunk() {
        let dir = TempDir::new("huge");
        let mut file = String::from("fn main() {\n");
        for i in 0..300 {
            file.push_str(&format!("    line{i}();\n"));
        }
        file.push_str("    added();\n}\n");
        write(&dir, "src/main.rs", &file);
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -300,3 +300,4 @@
     line298();
     line299();
+    added();
 }
";
        let out = extract_one(&dir.path().to_string_lossy(), diff);
        let u = &out[0].1[0];
        assert!(u.scope.is_none(), "a {MAX_SCOPE_LINES}+ line scope is not an excerpt");
        assert!(u.context.contains(">  302| "), "{}", u.context);
    }

    // -- scope detection ----------------------------------------------------

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(|s| s.to_string()).collect()
    }

    #[test]
    fn brace_scope_finds_the_function_not_the_if() {
        let src = lines(
            "use x;\n\nimpl Foo {\n    /// Doc.\n    #[inline]\n    pub fn bar(&self) -> u32 {\n        if self.ready {\n            self.count += 1;\n        }\n        self.count\n    }\n}\n",
        );
        let spec = crate::comments::lang_for("a.rs");
        let s = scope_for(&src, spec.as_ref(), "Rust", 7).expect("scope");
        // The `if` block does not qualify; the fn does — with its doc comment
        // and attribute attached.
        assert_eq!(s.header, "pub fn bar(&self) -> u32");
        assert_eq!((s.start0, s.end0), (3, 10));
    }

    #[test]
    fn brace_scope_handles_braces_in_strings_and_chars() {
        let src = lines(
            "fn a() {\n    let s = \"}}}{\";\n    let c = '{';\n    work();\n}\nfn b() {\n    other();\n}\n",
        );
        let spec = crate::comments::lang_for("a.rs");
        let s = scope_for(&src, spec.as_ref(), "Rust", 3).expect("scope");
        assert_eq!(s.header, "fn a()");
        assert_eq!((s.start0, s.end0), (0, 4));
    }

    #[test]
    fn brace_scope_covers_keywordless_c_functions() {
        let src = lines(
            "#include <stdio.h>\n\nstatic int\nhelper(int a,\n       int b)\n{\n    return a + b;\n}\n",
        );
        let spec = crate::comments::lang_for("a.c");
        let s = scope_for(&src, spec.as_ref(), "C/C++", 6).expect("scope");
        // Brace on its own line: the signature above it is the header, and a
        // multi-line signature is walked all the way up.
        assert_eq!((s.start0, s.end0), (2, 7));
    }

    #[test]
    fn go_methods_and_top_level_blocks_qualify() {
        let src = lines(
            "package main\n\nfunc (s *Server) Handle(w http.ResponseWriter) {\n\tif s.ok {\n\t\tserve(w)\n\t}\n}\n",
        );
        let spec = crate::comments::lang_for("a.go");
        let s = scope_for(&src, spec.as_ref(), "Go", 4).expect("scope");
        assert!(s.header.starts_with("func (s *Server) Handle"), "{}", s.header);
    }

    #[test]
    fn indent_scope_finds_the_python_method() {
        let src = lines(
            "import os\n\nclass Greeter:\n    @property\n    def name(self):\n        if self.formal:\n            return self.title\n\n        return self.nick\n\nTOP = 1\n",
        );
        let s = scope_for(&src, None, "Python", 8).expect("scope");
        // Nearest enclosing def, decorator included, trailing blank excluded.
        assert_eq!(s.header, "def name(self)");
        assert_eq!((s.start0, s.end0), (3, 8));
    }

    #[test]
    fn indent_scope_at_module_level_is_none() {
        let src = lines("import os\n\nX = 1\n");
        assert!(scope_for(&src, None, "Python", 2).is_none());
    }

    #[test]
    fn a_changed_def_line_scopes_to_itself() {
        let src = lines("class A:\n    def f(self, x):\n        return x\n");
        let s = scope_for(&src, None, "Python", 1).expect("scope");
        assert_eq!(s.header, "def f(self, x)");
        assert_eq!((s.start0, s.end0), (1, 2));
    }

    #[test]
    fn definition_headers_are_recognised_and_control_flow_is_not() {
        assert!(is_definition_header("pub async fn go() {"));
        assert!(is_definition_header("public static void main(String[] args) {"));
        assert!(is_definition_header("int main(void) {"));
        assert!(is_definition_header("export default function App() {"));
        assert!(is_definition_header("constructor(props) {"));
        assert!(!is_definition_header("if ready(now) {"));
        assert!(!is_definition_header("for (int i = 0; i < n; i++) {"));
        assert!(!is_definition_header("} else {"));
        assert!(!is_definition_header("return call(x)"));
    }

    #[test]
    fn the_prompt_names_the_region_and_the_scope() {
        let dir = TempDir::new("prompt");
        write(&dir, "src/main.rs", RS_FILE);
        let out = extract_one(&dir.path().to_string_lossy(), RS_DIFF);
        let p = build_prompt(&out[0].1[0]);
        assert!(p.contains("src/main.rs"), "{p}");
        assert!(p.contains("lines 12-13"), "{p}");
        assert!(p.contains("fn main()"), "{p}");
        assert!(p.contains("\"approve|revise|flag|delete\""), "{p}");
        assert!(p.contains("\"evidence\""), "{p}");
        assert!(p.contains("root of the repository"), "{p}");
    }
}
