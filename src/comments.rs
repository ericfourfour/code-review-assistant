//! Extract reviewable comment units from diff hunks.
//!
//! A "comment unit" is a run of consecutive whole-line comments in the new
//! side of a hunk that contains at least one added line — i.e. comments the
//! branch under review introduced or touched. Trailing comments that share a
//! line with code are out of scope for v1.

use serde::{Deserialize, Serialize};

use crate::diffparse::{DiffFile, Hunk};

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum CommentStyle {
    /// The prefix actually used by the original comment (e.g. `//`, `///`, `#`).
    Line { prefix: String },
    Block { open: String, close: String },
}

pub struct LangSpec {
    pub name: &'static str,
    pub line_prefixes: Vec<&'static str>,
    pub block: Option<(&'static str, &'static str)>,
}

pub fn lang_for(path: &str) -> Option<LangSpec> {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let spec = |name, mut line: Vec<&'static str>, block| {
        // longest-first so `///` wins over `//`
        line.sort_by_key(|p: &&str| std::cmp::Reverse(p.len()));
        Some(LangSpec { name, line_prefixes: line, block })
    };
    match ext.as_str() {
        "rs" => spec("Rust", vec!["///", "//!", "//"], Some(("/*", "*/"))),
        "c" | "h" | "cpp" | "cc" | "hpp" | "cxx" => spec("C/C++", vec!["///", "//"], Some(("/*", "*/"))),
        "js" | "jsx" | "mjs" | "cjs" => spec("JavaScript", vec!["//"], Some(("/*", "*/"))),
        "ts" | "tsx" => spec("TypeScript", vec!["//"], Some(("/*", "*/"))),
        "java" | "kt" | "kts" | "scala" => spec("JVM", vec!["///", "//"], Some(("/*", "*/"))),
        "go" => spec("Go", vec!["//"], Some(("/*", "*/"))),
        "swift" => spec("Swift", vec!["///", "//"], Some(("/*", "*/"))),
        "cs" => spec("C#", vec!["///", "//"], Some(("/*", "*/"))),
        "php" => spec("PHP", vec!["//", "#"], Some(("/*", "*/"))),
        "css" | "scss" | "less" => spec("CSS", vec!["//"], Some(("/*", "*/"))),
        "py" | "pyi" => spec("Python", vec!["#"], None),
        "rb" => spec("Ruby", vec!["#"], None),
        "sh" | "bash" | "zsh" => spec("Shell", vec!["#"], None),
        "yaml" | "yml" => spec("YAML", vec!["#"], None),
        "toml" => spec("TOML", vec!["#"], None),
        "tf" | "hcl" => spec("HCL", vec!["#", "//"], Some(("/*", "*/"))),
        "sql" => spec("SQL", vec!["--"], Some(("/*", "*/"))),
        "lua" => spec("Lua", vec!["--"], None),
        "hs" => spec("Haskell", vec!["--"], Some(("{-", "-}"))),
        "html" | "htm" | "xml" | "vue" | "svelte" => spec("Markup", vec![], Some(("<!--", "-->"))),
        _ => None,
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CommentUnit {
    pub file: String,
    pub lang: String,
    /// 1-based line numbers in the new (working-tree) version of the file.
    pub start_line: u32,
    pub end_line: u32,
    /// The exact file lines of the comment, indentation included.
    pub raw_lines: Vec<String>,
    /// Leading whitespace of the first comment line.
    pub indent: String,
    /// Line-comment prefix to use when reformatting a model rewrite
    /// (e.g. "// ", "# "). Block comments keep their own open/close.
    pub style: CommentStyle,
    /// Rendered context (numbered new-side lines, unit marked with `>`).
    pub context: String,
    pub hunk_header: String,
    /// True if any line of the unit is an added (+) line.
    pub has_added: bool,
}

impl CommentUnit {
    /// The unit's text with its own indentation stripped, for display and
    /// editing. Editing at column 0 keeps the comment readable in a narrow
    /// pane; [`Self::reindent`] puts the indentation back on the way to disk.
    /// Relative indentation *inside* the comment is preserved.
    pub fn dedent(&self, text: &str) -> String {
        let width = self.indent.chars().count();
        text.lines()
            .map(|l| {
                let strip = l.chars().take(width).take_while(|c| c.is_whitespace()).count();
                l.chars().skip(strip).collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Inverse of [`Self::dedent`]: raw file lines for dedented editor text.
    pub fn reindent(&self, text: &str) -> Vec<String> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        text.lines()
            .map(|l| {
                if l.trim().is_empty() {
                    String::new()
                } else {
                    format!("{}{l}", self.indent)
                }
            })
            .collect()
    }

    /// Reformat replacement prose into raw file lines matching the unit's
    /// original style and indentation.
    pub fn format_replacement(&self, prose: &str) -> Vec<String> {
        let prose = prose.trim();
        if prose.is_empty() {
            return Vec::new();
        }
        match &self.style {
            CommentStyle::Line { prefix } => {
                prose
                    .lines()
                    .map(|l| {
                        if l.trim().is_empty() {
                            format!("{}{}", self.indent, prefix)
                        } else {
                            format!("{}{} {}", self.indent, prefix, l.trim_end())
                        }
                    })
                    .collect()
            }
            CommentStyle::Block { open, close } => {
                let lines: Vec<&str> = prose.lines().collect();
                if lines.len() == 1 {
                    vec![format!("{}{} {} {}", self.indent, open, lines[0].trim(), close)]
                } else {
                    let mut out = vec![format!("{}{}", self.indent, open)];
                    for l in &lines {
                        out.push(format!("{} {}", self.indent, l.trim_end()));
                    }
                    out.push(format!("{}{}", self.indent, close));
                    out
                }
            }
        }
    }
}

/// Which line prefix (if any) makes this a whole-line comment start.
fn line_comment_prefix<'a>(text: &str, spec: &'a LangSpec) -> Option<&'a &'static str> {
    let t = text.trim_start();
    spec.line_prefixes.iter().find(|p| t.starts_with(**p))
}

fn indent_of(text: &str) -> String {
    text[..text.len() - text.trim_start().len()].to_string()
}

pub fn extract_units(files: &[DiffFile], context_lines: usize) -> Vec<(String, Vec<CommentUnit>)> {
    let mut out: Vec<(String, Vec<CommentUnit>)> = Vec::new();
    for f in files {
        let Some(spec) = lang_for(&f.path) else { continue };
        let mut units = Vec::new();
        for hunk in &f.hunks {
            units.extend(units_in_hunk(&f.path, &spec, hunk, context_lines));
        }
        // Only units the branch actually touched are reviewable.
        units.retain(|u| u.has_added);
        if !units.is_empty() {
            out.push((f.path.clone(), units));
        }
    }
    out
}

fn units_in_hunk(path: &str, spec: &LangSpec, hunk: &Hunk, context_lines: usize) -> Vec<CommentUnit> {
    // Work over the new-side lines only (' ' and '+').
    let new_side: Vec<(usize, &crate::diffparse::HunkLine)> = hunk
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.origin != '-')
        .collect();

    let mut units = Vec::new();
    let mut i = 0usize;
    while i < new_side.len() {
        let (_, line) = new_side[i];
        // Block comment start?
        if let Some((open, close)) = spec.block {
            let t = line.text.trim_start();
            if t.starts_with(open) && !is_line_prefixed(t, spec) {
                let mut j = i;
                let mut closed = t.contains(close) && t.find(close) > t.find(open);
                while !closed && j + 1 < new_side.len() {
                    j += 1;
                    if new_side[j].1.text.contains(close) {
                        closed = true;
                    }
                }
                if closed {
                    units.push(make_unit(
                        path,
                        spec,
                        hunk,
                        &new_side,
                        i,
                        j,
                        CommentStyle::Block { open: open.to_string(), close: close.to_string() },
                        context_lines,
                    ));
                    i = j + 1;
                    continue;
                }
            }
        }
        // Run of whole-line comments?
        if let Some(prefix) = line_comment_prefix(&line.text, spec) {
            let prefix = prefix.to_string();
            let mut j = i;
            while j + 1 < new_side.len()
                && line_comment_prefix(&new_side[j + 1].1.text, spec).is_some()
            {
                j += 1;
            }
            units.push(make_unit(
                path,
                spec,
                hunk,
                &new_side,
                i,
                j,
                CommentStyle::Line { prefix },
                context_lines,
            ));
            i = j + 1;
            continue;
        }
        i += 1;
    }
    units
}

fn is_line_prefixed(trimmed: &str, spec: &LangSpec) -> bool {
    spec.line_prefixes.iter().any(|p| trimmed.starts_with(p))
}

#[allow(clippy::too_many_arguments)]
fn make_unit(
    path: &str,
    spec: &LangSpec,
    hunk: &Hunk,
    new_side: &[(usize, &crate::diffparse::HunkLine)],
    i: usize,
    j: usize,
    style: CommentStyle,
    context_lines: usize,
) -> CommentUnit {
    let start_line = new_side[i].1.new_lineno.unwrap_or(1);
    let end_line = new_side[j].1.new_lineno.unwrap_or(start_line);
    let raw_lines: Vec<String> = new_side[i..=j].iter().map(|(_, l)| l.text.clone()).collect();
    let has_added = new_side[i..=j].iter().any(|(_, l)| l.origin == '+');

    // Context: new-side lines around the unit, numbered, unit rows marked.
    let lo = i.saturating_sub(context_lines);
    let hi = (j + context_lines).min(new_side.len().saturating_sub(1));
    let mut ctx = String::new();
    for (k, (_, l)) in new_side.iter().enumerate().take(hi + 1).skip(lo) {
        let marker = if k >= i && k <= j { '>' } else { ' ' };
        let no = l.new_lineno.map(|n| n.to_string()).unwrap_or_default();
        ctx.push_str(&format!("{marker}{no:>5}| {}\n", l.text));
    }

    CommentUnit {
        file: path.to_string(),
        lang: spec.name.to_string(),
        start_line,
        end_line,
        indent: indent_of(&new_side[i].1.text),
        raw_lines,
        style,
        context: ctx,
        hunk_header: hunk.header.clone(),
        has_added,
    }
}

/// The prompt sent to each model CLI. We assume the models already know what a
/// good comment looks like; we supply the code, the ask, and the fact that the
/// rest of the repository is theirs to read.
///
/// The hunk on its own is often too little to judge by. A comment that seems to
/// restate its line is doing real work if the thing it names is defined three
/// files away; one that reads like noise is load-bearing if it records why the
/// obvious implementation was rejected. Either call needs the surrounding code,
/// so the CLI is started in the repository root with read-only tools, and the
/// path below is relative to that root.
pub fn build_prompt(unit: &CommentUnit) -> String {
    format!(
        "File: {} ({})\n\n{}\nReview the comment marked with '>' (lines {}-{}). \
Should it be kept, rewritten, or deleted?\n\n\
You are running in the root of the repository this comment lives in, and the \
path above is relative to it. Read whatever you need before deciding: the rest \
of the file, what it calls, who calls it, the tests. Judge the comment against \
the code as it actually is — delete one that only restates the line below it, \
keep one that carries something the code cannot say for itself.\n\n{}",
        unit.file,
        unit.lang,
        unit.context,
        unit.start_line,
        unit.end_line,
        crate::models::answer_schema(false)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units_for(diff: &str) -> Vec<(String, Vec<CommentUnit>)> {
        let files = crate::diffparse::parse(diff);
        extract_units(&files, 12)
    }

    const RS_DIFF: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,7 @@
 fn main() {
+    // Increment the counter by one
+    // to increment the counter.
+    counter += 1;
     // pre-existing untouched comment
     println!(\"hi\");
 }
";

    #[test]
    fn extracts_only_touched_comment_runs() {
        let out = units_for(RS_DIFF);
        assert_eq!(out.len(), 1);
        let units = &out[0].1;
        // the pre-existing context-only comment must not be reviewable
        assert_eq!(units.len(), 1);
        let u = &units[0];
        assert_eq!((u.start_line, u.end_line), (2, 3));
        assert!(u.has_added);
        assert_eq!(u.raw_lines.len(), 2);
        assert!(u.context.contains(">    2| "));
    }

    #[test]
    fn python_hash_comments() {
        let diff = "\
diff --git a/a.py b/a.py
--- a/a.py
+++ b/a.py
@@ -1,2 +1,3 @@
 x = 1
+# add one to x
 y = x + 1
";
        let out = units_for(diff);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1[0].raw_lines[0], "# add one to x");
    }

    #[test]
    fn block_comment_unit() {
        let diff = "\
diff --git a/a.c b/a.c
--- a/a.c
+++ b/a.c
@@ -1,2 +1,5 @@
 int x;
+/* This variable
+   holds a number */
+int y;
 int z;
";
        let out = units_for(diff);
        assert_eq!(out.len(), 1);
        let u = &out[0].1[0];
        assert!(matches!(u.style, CommentStyle::Block { .. }));
        assert_eq!((u.start_line, u.end_line), (2, 3));
    }

    #[test]
    fn format_replacement_line_style() {
        let out = units_for(RS_DIFF);
        let u = &out[0].1[0];
        let lines = u.format_replacement("Bump the counter.");
        assert_eq!(lines, vec!["    // Bump the counter.".to_string()]);
        assert!(u.format_replacement("  ").is_empty());
    }

    #[test]
    fn dedent_reindent_round_trip() {
        let out = units_for(RS_DIFF);
        let u = &out[0].1[0];
        let flat = u.dedent(&u.raw_lines.join("\n"));
        assert_eq!(flat, "// Increment the counter by one\n// to increment the counter.");
        assert_eq!(u.reindent(&flat), u.raw_lines);
        // deeper relative indentation inside the comment survives the trip
        let nested = "// a\n//   b";
        assert_eq!(u.dedent(&u.reindent(nested).join("\n")), nested);
        assert!(u.reindent("  \n").is_empty());
    }

    #[test]
    fn prompt_is_contextual_and_opens_the_repo() {
        let out = units_for(RS_DIFF);
        let p = build_prompt(&out[0].1[0]);
        assert!(p.contains("src/lib.rs"));
        assert!(p.contains("lines 2-3"));
        assert!(p.contains("\"action\""));
        // A path is only useful to the model once it knows what it is relative
        // to, and it will not go looking unless it is told it may.
        assert!(p.contains("root of the repository"), "{p}");
        assert!(p.contains("relative to it"), "{p}");
        assert!(p.len() < 2500);
    }
}
