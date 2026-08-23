//! One reviewable unit — a comment run or a code change — and the plan
//! assembly that merges the two extractors into a single ordered walk.

use serde::{Deserialize, Serialize};

use crate::codeunits::CodeUnit;
use crate::comments::CommentUnit;
use crate::diffparse::DiffFile;
use crate::models::Action;

/// UI label for a verdict, phrased for what was reviewed: a code unit's Keep
/// is an "approve" and its Rewrite a "revise" — the words the code prompt
/// itself uses. Found by this app's own branch pass: the badges said KEEP
/// next to previews saying "approve — sound as written".
pub fn action_label(action: Action, kind: UnitKind) -> &'static str {
    match (kind, action) {
        (UnitKind::Code, Action::Keep) => "APPROVE",
        (UnitKind::Code, Action::Rewrite) => "REVISE",
        _ => action.label(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    Comment,
    Code,
}

/// Untagged on purpose: decisions recorded before code units existed store a
/// bare `CommentUnit` in `unit_json`, and they must keep loading. The two
/// shapes are told apart by their required fields (`style`/`indent` vs
/// `changed_lines`), so no tag is needed going forward either.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReviewUnit {
    Comment(CommentUnit),
    Code(CodeUnit),
}

impl ReviewUnit {
    pub fn kind(&self) -> UnitKind {
        match self {
            ReviewUnit::Comment(_) => UnitKind::Comment,
            ReviewUnit::Code(_) => UnitKind::Code,
        }
    }

    pub fn is_code(&self) -> bool {
        matches!(self, ReviewUnit::Code(_))
    }

    pub fn file(&self) -> &str {
        match self {
            ReviewUnit::Comment(u) => &u.file,
            ReviewUnit::Code(u) => &u.file,
        }
    }

    pub fn lang(&self) -> &str {
        match self {
            ReviewUnit::Comment(u) => &u.lang,
            ReviewUnit::Code(u) => &u.lang,
        }
    }

    pub fn start_line(&self) -> u32 {
        match self {
            ReviewUnit::Comment(u) => u.start_line,
            ReviewUnit::Code(u) => u.start_line,
        }
    }

    pub fn end_line(&self) -> u32 {
        match self {
            ReviewUnit::Comment(u) => u.end_line,
            ReviewUnit::Code(u) => u.end_line,
        }
    }

    pub fn raw_lines(&self) -> &[String] {
        match self {
            ReviewUnit::Comment(u) => &u.raw_lines,
            ReviewUnit::Code(u) => &u.raw_lines,
        }
    }

    pub fn context(&self) -> &str {
        match self {
            ReviewUnit::Comment(u) => &u.context,
            ReviewUnit::Code(u) => &u.context,
        }
    }

    pub fn hunk_header(&self) -> &str {
        match self {
            ReviewUnit::Comment(u) => &u.hunk_header,
            ReviewUnit::Code(u) => &u.hunk_header,
        }
    }

    /// Short badge text: what kind of unit this is, and for code, how much
    /// the excerpt shows.
    pub fn kind_label(&self) -> String {
        match self {
            ReviewUnit::Comment(_) => "comment".into(),
            ReviewUnit::Code(u) => match &u.scope {
                Some(s) => format!("code · {s}"),
                None => "code · hunk".into(),
            },
        }
    }

    /// What the editor shows. Comments edit flush left (their indentation is
    /// restored on save); code edits exactly as it sits in the file, because
    /// indentation *is* code.
    pub fn display_text(&self) -> String {
        match self {
            ReviewUnit::Comment(u) => u.dedent(&u.raw_lines.join("\n")),
            ReviewUnit::Code(u) => u.raw_lines.join("\n"),
        }
    }

    /// Inverse of [`Self::display_text`]: raw file lines for the editor's
    /// content. Empty (whitespace-only) means delete.
    pub fn editor_to_lines(&self, editor: &str) -> Vec<String> {
        match self {
            ReviewUnit::Comment(u) => u.reindent(editor),
            ReviewUnit::Code(_) => {
                if editor.trim().is_empty() {
                    Vec::new()
                } else {
                    editor.lines().map(|l| l.to_string()).collect()
                }
            }
        }
    }

    /// A model's proposed replacement, as editor text. Comment rewrites are
    /// prose reformatted into the unit's comment style; code revisions are
    /// taken verbatim.
    pub fn replacement_display(&self, proposal: &str) -> String {
        match self {
            ReviewUnit::Comment(u) => u.dedent(&u.format_replacement(proposal).join("\n")),
            ReviewUnit::Code(_) => proposal.trim_end_matches('\n').to_string(),
        }
    }

    pub fn build_prompt(&self) -> String {
        match self {
            ReviewUnit::Comment(u) => crate::comments::build_prompt(u),
            ReviewUnit::Code(u) => crate::codeunits::build_prompt(u),
        }
    }
}

/// Everything reviewable in the diff, per file, in walk order. Comment units
/// that fall inside a code unit's region are absorbed by it — the units in a
/// file must stay disjoint and ordered or the line-offset bookkeeping that
/// lets sequential edits land correctly falls apart, and the code verdict
/// covers the comments it encloses anyway.
pub fn assemble(
    repo: &str,
    files: &[DiffFile],
    context_lines: usize,
    include_comments: bool,
    include_code: bool,
) -> Vec<(String, Vec<ReviewUnit>)> {
    let comment_files = if include_comments {
        crate::comments::extract_units(files, context_lines)
    } else {
        Vec::new()
    };
    let code_files = if include_code {
        crate::codeunits::extract(repo, files, context_lines)
    } else {
        Vec::new()
    };

    let mut out: Vec<(String, Vec<ReviewUnit>)> = Vec::new();
    for f in files {
        let code: Vec<CodeUnit> = code_files
            .iter()
            .find(|(p, _)| p == &f.path)
            .map(|(_, u)| u.clone())
            .unwrap_or_default();
        let mut units: Vec<ReviewUnit> = Vec::new();
        if let Some((_, comments)) = comment_files.iter().find(|(p, _)| p == &f.path) {
            for c in comments {
                let absorbed = code
                    .iter()
                    .any(|k| c.start_line <= k.end_line && k.start_line <= c.end_line);
                if !absorbed {
                    units.push(ReviewUnit::Comment(c.clone()));
                }
            }
        }
        units.extend(code.into_iter().map(ReviewUnit::Code));
        units.sort_by_key(|u| u.start_line());
        if !units.is_empty() {
            out.push((f.path.clone(), units));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;

    const FILE: &str = "\
fn main() {
    let name = read();
    // greet politely
    greet(name);
}

// A standalone note about configuration
const RETRIES: u32 = 3;
";

    const DIFF: &str = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,2 +1,5 @@
 fn main() {
+    let name = read();
+    // greet politely
+    greet(name);
 }
@@ -4,1 +7,2 @@
+// A standalone note about configuration
 const RETRIES: u32 = 3;
";

    fn repo_with_file() -> TempDir {
        let dir = TempDir::new("assemble");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), FILE).unwrap();
        dir
    }

    #[test]
    fn comments_inside_a_code_region_are_absorbed_and_standalone_ones_kept() {
        let dir = repo_with_file();
        let files = crate::diffparse::parse(DIFF);
        let out = assemble(&dir.path().to_string_lossy(), &files, 12, true, true);
        assert_eq!(out.len(), 1);
        let units = &out[0].1;
        // The comment inside the changed code is the code unit's to judge; the
        // standalone comment near untouched code keeps its own comment unit.
        assert_eq!(units.len(), 2, "{:?}", kinds(units));
        assert!(units[0].is_code());
        assert_eq!((units[0].start_line(), units[0].end_line()), (2, 4));
        assert!(!units[1].is_code());
        assert_eq!(units[1].start_line(), 7);
        // Ordered and disjoint, or sequential edits would corrupt offsets.
        assert!(units[0].end_line() < units[1].start_line());
    }

    #[test]
    fn toggles_select_which_units_exist() {
        let dir = repo_with_file();
        let files = crate::diffparse::parse(DIFF);
        let comments_only = assemble(&dir.path().to_string_lossy(), &files, 12, true, false);
        assert!(comments_only[0].1.iter().all(|u| !u.is_code()));
        assert_eq!(comments_only[0].1.len(), 2, "both comment runs, nothing absorbs them");
        let code_only = assemble(&dir.path().to_string_lossy(), &files, 12, false, true);
        assert!(code_only[0].1.iter().all(|u| u.is_code()));
        assert_eq!(code_only[0].1.len(), 1);
    }

    #[test]
    fn verdict_labels_speak_the_unit_kind() {
        assert_eq!(action_label(Action::Keep, UnitKind::Code), "APPROVE");
        assert_eq!(action_label(Action::Rewrite, UnitKind::Code), "REVISE");
        assert_eq!(action_label(Action::Delete, UnitKind::Code), "DELETE");
        assert_eq!(action_label(Action::Flag, UnitKind::Code), "FLAG");
        assert_eq!(action_label(Action::Keep, UnitKind::Comment), "KEEP");
        assert_eq!(action_label(Action::Rewrite, UnitKind::Comment), "REWRITE");
    }

    #[test]
    fn old_comment_unit_json_still_loads() {
        // A decision recorded before ReviewUnit existed stores a bare
        // CommentUnit object; the untagged enum must keep reading it.
        let old = r#"{"file":"src/lib.rs","lang":"Rust","start_line":2,"end_line":3,
            "raw_lines":["    // a","    // b"],"indent":"    ",
            "style":{"Line":{"prefix":"//"}},"context":"ctx","hunk_header":"@@","has_added":true}"#;
        let u: ReviewUnit = serde_json::from_str(old).expect("parse old json");
        assert!(!u.is_code());
        assert_eq!(u.file(), "src/lib.rs");
        assert_eq!((u.start_line(), u.end_line()), (2, 3));
    }

    #[test]
    fn both_kinds_round_trip_through_json() {
        let dir = repo_with_file();
        let files = crate::diffparse::parse(DIFF);
        let out = assemble(&dir.path().to_string_lossy(), &files, 12, true, true);
        for unit in &out[0].1 {
            let json = serde_json::to_string(unit).unwrap();
            let back: ReviewUnit = serde_json::from_str(&json).unwrap();
            assert_eq!(back.is_code(), unit.is_code(), "{json}");
            assert_eq!(back.start_line(), unit.start_line());
            assert_eq!(back.raw_lines(), unit.raw_lines());
        }
    }

    #[test]
    fn code_edits_round_trip_verbatim_and_comment_edits_reindent() {
        let dir = repo_with_file();
        let files = crate::diffparse::parse(DIFF);
        let out = assemble(&dir.path().to_string_lossy(), &files, 12, true, true);
        let code = &out[0].1[0];
        assert_eq!(code.display_text(), code.raw_lines().join("\n"), "code shows as-is");
        assert_eq!(
            code.editor_to_lines(&code.display_text()),
            code.raw_lines(),
            "untouched code saves back exactly"
        );
        assert!(code.editor_to_lines("   \n").is_empty(), "blank editor deletes");
        assert_eq!(code.replacement_display("    x();\n"), "    x();");

        let comment = &out[0].1[1];
        assert_eq!(comment.display_text(), "// A standalone note about configuration");
        assert_eq!(
            comment.editor_to_lines("// tightened"),
            vec!["// tightened".to_string()],
            "top-level comment has no indent to restore"
        );
    }

    fn kinds(units: &[ReviewUnit]) -> Vec<String> {
        units.iter().map(|u| u.kind_label()).collect()
    }
}
