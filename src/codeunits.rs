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
/// single 993-line unit before this existed). Splits cut at definition tops
/// where the language is parsable, blank lines otherwise.
const MAX_REGION_LINES: u32 = 120;

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
    /// The unit comes from the old side of a fully deleted file. It can be
    /// approved or flagged, but there is no working-tree file to splice.
    #[serde(default)]
    pub deleted_file: bool,
}

pub fn extract(
    repo: &str,
    files: &[DiffFile],
    context_lines: usize,
) -> Vec<(String, Vec<CodeUnit>)> {
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
    let lang = spec
        .as_ref()
        .map(|s| s.name.to_string())
        .unwrap_or_else(|| lang_fallback(&f.path));
    if f.deleted {
        return deleted_file_units(f, &lang, spec.as_ref());
    }

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

    let disk_src: Option<String> = std::fs::read_to_string(Path::new(repo).join(&f.path)).ok();
    let disk: Option<Vec<String>> = disk_src
        .as_ref()
        .map(|c| c.lines().map(|s| s.to_string()).collect());
    // One real parse per file; every semantic question below goes through it.
    let parsed: Option<crate::scopes::ParsedFile> = disk_src
        .as_deref()
        .and_then(|src| crate::scopes::parse(&f.path, src));

    let mut units = Vec::new();
    for hunk in &f.hunks {
        let span: Vec<u32> = hunk.lines.iter().filter_map(|l| l.new_lineno).collect();
        let (Some(&first_new), Some(&last_new)) = (span.first(), span.last()) else {
            continue;
        };

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
                    hunk.lines
                        .get(idx)
                        .and_then(|n| n.new_lineno)
                        .unwrap_or(first_new)
                };
                positions.insert(anchor.clamp(first_new, last_new));
            }
        }
        if positions.is_empty() {
            continue;
        }

        let windows: Vec<(u32, u32)> = clusters(&positions, CLUSTER_GAP)
            .into_iter()
            .flat_map(|c| split_region(c, &new_text, disk.as_deref(), parsed.as_ref()))
            .collect();
        for (rs, re) in windows {
            let raw_lines: Vec<String> =
                match (rs..=re).map(|no| new_text.get(&no).cloned()).collect() {
                    Some(lines) => lines,
                    None => continue, // hole in the diff — should not happen
                };
            let changed_lines: Vec<u32> = added.range(rs..=re).copied().collect();
            let (scope, context) = context_for(
                disk.as_deref(),
                parsed.as_ref(),
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
                deleted_file: false,
            });
        }
    }
    units
}

/// Review the old side of a fully deleted file in bounded windows. There is
/// no new-side anchor and no file on disk, so ordinary extraction cannot
/// discover a unit; the removed lines themselves are the review material.
fn deleted_file_units(f: &DiffFile, lang: &str, spec: Option<&LangSpec>) -> Vec<CodeUnit> {
    let mut out = Vec::new();
    for hunk in &f.hunks {
        let removed: Vec<_> = hunk
            .lines
            .iter()
            .filter(|line| line.origin == '-' && line.old_lineno.is_some())
            .collect();
        for window in removed.chunks(MAX_REGION_LINES as usize) {
            if !window.iter().any(|line| removed_is_code(spec, &line.text)) {
                continue;
            }
            let start_line = window.first().and_then(|line| line.old_lineno).unwrap_or(1);
            let end_line = window
                .last()
                .and_then(|line| line.old_lineno)
                .unwrap_or(start_line);
            let raw_lines = window.iter().map(|line| line.text.clone()).collect();
            let mut context = format!("{}\n", hunk.header);
            for line in window {
                context.push_str(&format!(
                    ">{:>5}| -{}\n",
                    line.old_lineno.unwrap_or(0),
                    line.text
                ));
            }
            out.push(CodeUnit {
                file: f.path.clone(),
                lang: lang.to_string(),
                start_line,
                end_line,
                raw_lines,
                changed_lines: Vec::new(),
                scope: None,
                context,
                hunk_header: hunk.header.clone(),
                deleted_file: true,
            });
        }
    }
    out
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
/// [`MAX_REGION_LINES`]. Semantic boundaries come first: when the file has a
/// grammar and the checkout matches the diff, windows cut at the tops of the
/// definitions the region spans, descending into oversized containers (an
/// `impl` block splits at its methods, a class at its defs). Blank lines are
/// the fallback — inside a definition that is itself too long, and in files
/// no bundled grammar can parse.
fn split_region(
    region: (u32, u32),
    new_text: &BTreeMap<u32, String>,
    disk: Option<&[String]>,
    parsed: Option<&crate::scopes::ParsedFile>,
) -> Vec<(u32, u32)> {
    let (s, e) = region;
    if e - s < MAX_REGION_LINES {
        return vec![region];
    }
    if let (Some(disk), Some(parsed)) = (disk, parsed) {
        let matches = (s..=e).all(|no| {
            disk.get(no as usize - 1).map(|x| x.as_str()) == new_text.get(&no).map(|x| x.as_str())
        });
        if matches {
            if let Some(windows) = semantic_split(disk, parsed, region) {
                return windows;
            }
        }
    }
    blank_split(region, &|no| {
        new_text.get(&no).is_some_and(|t| t.trim().is_empty())
    })
}

/// Cut the region into definition and gap segments, blank-split any segment
/// still over the cap, then pack consecutive segments greedily back up to it.
/// A cut can therefore only land at a definition top, at a blank line inside
/// an oversized definition, or at a blank line in unparsed territory — never
/// mid-function when the parse succeeded.
fn semantic_split(
    disk: &[String],
    parsed: &crate::scopes::ParsedFile,
    region: (u32, u32),
) -> Option<Vec<(u32, u32)>> {
    let (s, e) = region;
    let items = parsed.definition_spans(s as usize - 1, e as usize - 1, MAX_REGION_LINES as usize);
    let mut segs: Vec<(u32, u32)> = Vec::new();
    let mut cursor = s;
    for (a0, b0) in items {
        let (a, b) = ((a0 as u32 + 1).max(cursor), (b0 as u32 + 1).min(e));
        if a > b || a > e {
            continue;
        }
        if a > cursor {
            segs.push((cursor, a - 1));
        }
        segs.push((a, b));
        cursor = b + 1;
        if cursor > e {
            break;
        }
    }
    if cursor <= e {
        segs.push((cursor, e));
    }
    if segs.len() <= 1 {
        return None; // no boundaries worth cutting at
    }
    let is_blank = |no: u32| {
        disk.get(no as usize - 1)
            .is_some_and(|t| t.trim().is_empty())
    };
    let mut pieces: Vec<(u32, u32)> = Vec::new();
    for seg in segs {
        pieces.extend(blank_split(seg, &is_blank));
    }
    let mut out: Vec<(u32, u32)> = Vec::new();
    let mut cur: Option<(u32, u32)> = None;
    for (a, b) in pieces {
        cur = match cur {
            None => Some((a, b)),
            Some((cs, _)) if b - cs < MAX_REGION_LINES => Some((cs, b)),
            Some(done) => {
                out.push(done);
                Some((a, b))
            }
        };
    }
    out.extend(cur);
    Some(out)
}

/// Blank-line splitting, the last resort: windows of at most the cap, each
/// cut at a blank line in its back half when one exists.
fn blank_split(region: (u32, u32), is_blank: &dyn Fn(u32) -> bool) -> Vec<(u32, u32)> {
    let (mut start, end) = region;
    let mut out = Vec::new();
    while end - start + 1 > MAX_REGION_LINES {
        let hard = start + MAX_REGION_LINES - 1;
        let cut = (start + MAX_REGION_LINES / 2..=hard)
            .rev()
            .find(|no| is_blank(*no))
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
    parsed: Option<&crate::scopes::ParsedFile>,
    region: (u32, u32),
    hunk_span: (u32, u32),
    new_text: &BTreeMap<u32, String>,
    added: &BTreeSet<u32>,
    removals: &BTreeMap<u32, Vec<String>>,
    context_lines: usize,
) -> (Option<String>, String) {
    let (rs, re) = region;
    if let (Some(disk), Some(parsed)) = (disk, parsed) {
        // The diff and the working tree must agree before disk lines can be
        // shown as "the new side" — an already-dirty checkout must not put
        // words in the branch's mouth.
        let matches = (rs..=re).all(|no| {
            disk.get(no as usize - 1).map(|s| s.as_str()) == new_text.get(&no).map(|s| s.as_str())
        });
        if matches {
            // Anchor in the middle of the region: its first line may be the
            // doc comment or signature *above* the body, which no node
            // encloses, while the midpoint of a definition-aligned window
            // sits inside the definition itself.
            let mid = (rs + re) / 2;
            if let Some(scope) = parsed.scope_for(mid as usize - 1) {
                let start0 = scope.start0.min(rs as usize - 1);
                let end0 = scope
                    .end0
                    .max(re as usize - 1)
                    .min(disk.len().saturating_sub(1));
                if end0 - start0 < MAX_SCOPE_LINES {
                    let numbered: Vec<(u32, &str)> = (start0..=end0)
                        .map(|i| (i as u32 + 1, disk[i].as_str()))
                        .collect();
                    return (
                        Some(scope.header),
                        render(&numbered, region, added, removals),
                    );
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
        assert_eq!(
            units.len(),
            1,
            "both added lines share a scope and a cluster"
        );
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
        assert!(
            u.scope.is_none(),
            "no scope detection for unknown languages"
        );
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
        assert!(
            u.context.contains("-     | ") && u.context.contains("gone();"),
            "{}",
            u.context
        );
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
        let mut diff =
            String::from("diff --git a/a.js b/a.js\n--- a/a.js\n+++ b/a.js\n@@ -1,28 +1,30 @@\n");
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

    /// Shared checks for any split: bounded, consecutive, each reviewing
    /// something.
    fn assert_windows(units: &[CodeUnit]) {
        for (i, u) in units.iter().enumerate() {
            let len = u.end_line - u.start_line + 1;
            assert!(len <= MAX_REGION_LINES, "window {i} is {len} lines");
            assert!(!u.changed_lines.is_empty(), "window {i} reviews nothing");
            if i > 0 {
                assert_eq!(
                    u.start_line,
                    units[i - 1].end_line + 1,
                    "windows must stay consecutive"
                );
            }
        }
    }

    fn added_file_diff(path: &str, body: &str) -> String {
        let n = body.lines().count();
        let mut diff = format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -0,0 +1,{n} @@\n"
        );
        for line in body.lines() {
            diff.push_str(&format!("+{line}\n"));
        }
        diff
    }

    #[test]
    fn a_huge_new_rust_file_splits_at_function_tops_not_blank_lines() {
        let dir = TempDir::new("fnsplit");
        // Ten top-level functions, each with a doc line, big enough that one
        // window cannot hold them all.
        let mut body = String::new();
        for f in 0..10 {
            body.push_str(&format!("/// Does thing {f}.\nfn f{f}() {{\n"));
            for j in 0..21 {
                body.push_str(&format!("    step{j}();\n"));
            }
            body.push_str("}\n\n");
        }
        write(&dir, "src/gen.rs", &body);
        let out = extract_one(
            &dir.path().to_string_lossy(),
            &added_file_diff("src/gen.rs", &body),
        );
        let units = &out[0].1;
        assert!(
            units.len() >= 2,
            "{} lines must split",
            body.lines().count()
        );
        assert_windows(units);
        let lines: Vec<&str> = body.lines().collect();
        for u in &units[1..] {
            let first = lines[u.start_line as usize - 1].trim();
            // Every cut lands at a definition top (doc line or fn header) —
            // never mid-function, even though blank lines exist inside none
            // and between all of them.
            assert!(
                first.starts_with("///") || first.starts_with("fn "),
                "window starts mid-item: {first:?}"
            );
        }
    }

    #[test]
    fn an_oversized_impl_splits_at_its_methods() {
        let dir = TempDir::new("implsplit");
        let mut body = String::from("struct S;\n\nimpl S {\n");
        for k in 0..8 {
            body.push_str(&format!("    fn m{k}(&self) {{\n"));
            for j in 0..20 {
                body.push_str(&format!("        step{j}();\n"));
            }
            body.push_str("    }\n");
            if k < 7 {
                body.push('\n');
            }
        }
        body.push_str("}\n");
        write(&dir, "src/s.rs", &body);
        let out = extract_one(
            &dir.path().to_string_lossy(),
            &added_file_diff("src/s.rs", &body),
        );
        let units = &out[0].1;
        assert!(units.len() >= 2, "the impl is bigger than one window");
        assert_windows(units);
        let lines: Vec<&str> = body.lines().collect();
        for u in &units[1..] {
            let first = lines[u.start_line as usize - 1].trim();
            // The impl itself is over the cap, so the split must descend to
            // its methods rather than falling back to blank lines.
            assert!(
                first.starts_with("fn m"),
                "window starts mid-method: {first:?}"
            );
        }
    }

    #[test]
    fn an_oversized_python_module_splits_at_defs() {
        let dir = TempDir::new("pysplit");
        let mut body = String::from("import os\n\n");
        for d in 0..2 {
            body.push_str(&format!("def handler_{d}(request):\n"));
            for j in 0..68 {
                body.push_str(&format!("    step_{j}()\n"));
            }
            body.push('\n');
        }
        write(&dir, "svc.py", &body);
        let out = extract_one(
            &dir.path().to_string_lossy(),
            &added_file_diff("svc.py", &body),
        );
        let units = &out[0].1;
        assert!(units.len() >= 2);
        assert_windows(units);
        let lines: Vec<&str> = body.lines().collect();
        for u in &units[1..] {
            let first = lines[u.start_line as usize - 1].trim();
            assert!(
                first.starts_with("def "),
                "window starts mid-def: {first:?}"
            );
        }
    }

    #[test]
    fn an_unparsable_file_falls_back_to_blank_line_windows() {
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
        let mut diff = String::from(
            "diff --git a/big.js b/big.js\n--- a/big.js\n+++ b/big.js\n@@ -0,0 +1,300 @@\n",
        );
        for line in body.lines() {
            diff.push_str(&format!("+{line}\n"));
        }
        let out = extract_one(&dir.path().to_string_lossy(), &diff);
        let units = &out[0].1;
        assert!(
            units.len() >= 3,
            "300 added lines should not be one unit: {}",
            units.len()
        );
        for (i, u) in units.iter().enumerate() {
            let len = u.end_line - u.start_line + 1;
            assert!(len <= MAX_REGION_LINES, "window {i} is {len} lines");
            assert!(!u.changed_lines.is_empty(), "window {i} reviews nothing");
            if i > 0 {
                assert_eq!(
                    u.start_line,
                    units[i - 1].end_line + 1,
                    "windows must stay consecutive"
                );
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
        assert_eq!(
            units.last().unwrap().end_line,
            299,
            "the last content line ends the last window"
        );
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
        assert!(
            u.scope.is_none(),
            "a {MAX_SCOPE_LINES}+ line scope is not an excerpt"
        );
        assert!(u.context.contains(">  302| "), "{}", u.context);
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

    #[test]
    fn a_fully_deleted_file_becomes_reviewable_code_units() {
        let diff = "\
diff --git a/src/gone.rs b/src/gone.rs
deleted file mode 100644
--- a/src/gone.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-fn removed() {
-    dangerous();
-}
";
        let files = crate::diffparse::parse(diff);
        let extracted = extract("", &files, 12);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].0, "src/gone.rs");
        assert_eq!(extracted[0].1.len(), 1);
        let unit = &extracted[0].1[0];
        assert!(unit.deleted_file);
        assert_eq!(unit.start_line, 1);
        assert_eq!(unit.end_line, 3);
        assert!(unit.raw_lines.iter().any(|line| line.contains("dangerous")));
    }
}
