//! Review session state: the plan (files × comment units), edit application
//! to the working tree, provenance tracking, and commit message assembly.

use std::path::Path;

use crate::comments::CommentUnit;
use crate::models::Action;

#[derive(Clone, PartialEq)]
pub enum RefKind {
    Branch,
    Pr(u64),
    WorkingTree,
}

impl RefKind {
    pub fn label(&self) -> String {
        match self {
            RefKind::Branch => "branch".into(),
            RefKind::Pr(n) => format!("PR #{n}"),
            RefKind::WorkingTree => "working tree".into(),
        }
    }
}

pub struct ReviewFile {
    pub path: String,
    pub units: Vec<CommentUnit>,
    /// Line-number shift accumulated by earlier edits in this file.
    pub line_offset: i64,
    pub decided: usize,
}

pub struct ReviewPlan {
    pub session_id: i64,
    pub ref_kind: RefKind,
    pub ref_name: String,
    pub base_ref: String,
    pub files: Vec<ReviewFile>,
    pub file_idx: usize,
    pub unit_idx: usize,
    pub decided_total: usize,
}

impl ReviewPlan {
    pub fn total_units(&self) -> usize {
        self.files.iter().map(|f| f.units.len()).sum()
    }

    pub fn current(&self) -> Option<(&ReviewFile, &CommentUnit)> {
        let f = self.files.get(self.file_idx)?;
        let u = f.units.get(self.unit_idx)?;
        Some((f, u))
    }

    /// Advance to the next unit. Returns false when the plan is exhausted.
    pub fn advance(&mut self) -> bool {
        if self.file_idx >= self.files.len() {
            return false;
        }
        self.unit_idx += 1;
        while self.file_idx < self.files.len()
            && self.unit_idx >= self.files[self.file_idx].units.len()
        {
            self.file_idx += 1;
            self.unit_idx = 0;
        }
        self.file_idx < self.files.len()
    }

    /// Step back to the previous unit (navigation only; no undo).
    pub fn retreat(&mut self) -> bool {
        if self.unit_idx > 0 {
            self.unit_idx -= 1;
            return true;
        }
        let mut fi = self.file_idx;
        while fi > 0 {
            fi -= 1;
            if !self.files[fi].units.is_empty() {
                self.file_idx = fi;
                self.unit_idx = self.files[fi].units.len() - 1;
                return true;
            }
        }
        false
    }

    pub fn jump_to_file(&mut self, file_idx: usize) {
        self.file_idx = file_idx.min(self.files.len());
        self.unit_idx = 0;
    }

    pub fn file_progress(&self) -> (usize, usize) {
        match self.files.get(self.file_idx) {
            Some(f) => (self.unit_idx.min(f.units.len()), f.units.len()),
            None => (0, 0),
        }
    }
}

/// What the human picked (before free-form edits are considered).
#[derive(Clone, PartialEq)]
pub enum Choice {
    /// Candidate from model slot i (index into the session's model list).
    Candidate(usize),
    /// Explicit "keep the original text".
    KeepOriginal,
    /// Explicit "delete this comment".
    Delete,
}

/// Provenance of the final text, derived at save time.
pub enum Provenance {
    Unchanged,
    Model {
        name: String,
        coauthor: String,
        edited: bool,
    },
    Human,
}

impl Provenance {
    pub fn source_str(&self) -> String {
        match self {
            Provenance::Unchanged => "original".into(),
            Provenance::Model {
                name,
                edited: false,
                ..
            } => name.clone(),
            Provenance::Model {
                name, edited: true, ..
            } => format!("{name}+human-edited"),
            Provenance::Human => "human-authored".into(),
        }
    }
}

/// Decide provenance from what was chosen and what ended up in the editor.
pub fn derive_provenance(
    chosen: &Option<Choice>,
    chosen_model: Option<(&str, &str)>, // (name, coauthor)
    editor_text: &str,
    candidate_baseline: Option<&str>,
    original_text: &str,
) -> Provenance {
    if editor_text == original_text {
        return Provenance::Unchanged;
    }
    match (chosen, chosen_model) {
        (Some(Choice::Candidate(_)), Some((name, coauthor))) => {
            let edited = candidate_baseline.map(|b| b != editor_text).unwrap_or(true);
            Provenance::Model {
                name: name.to_string(),
                coauthor: coauthor.to_string(),
                edited,
            }
        }
        _ => Provenance::Human,
    }
}

/// The final action implied by the editor state.
pub fn final_action(editor_text: &str, original_text: &str) -> Action {
    if editor_text.trim().is_empty() {
        Action::Delete
    } else if editor_text == original_text {
        Action::Keep
    } else {
        Action::Rewrite
    }
}

/// Replace the unit's lines in the working-tree file with `new_lines`
/// (empty = delete). Returns the line-count delta for offset tracking.
pub fn apply_edit(
    repo: &str,
    file: &ReviewFile,
    unit: &CommentUnit,
    new_lines: &[String],
) -> Result<i64, String> {
    let path = Path::new(repo).join(&unit.file);
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let had_trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();

    let start = (unit.start_line as i64 - 1 + file.line_offset).max(0) as usize;
    let end = (unit.end_line as i64 - 1 + file.line_offset).max(0) as usize; // inclusive
    if end >= lines.len() {
        return Err(format!(
            "line range {}..{} out of bounds for {} ({} lines) — file changed on disk?",
            start + 1,
            end + 1,
            unit.file,
            lines.len()
        ));
    }
    // Sanity check: the file should still contain what we think it does.
    let on_disk: Vec<&String> = lines[start..=end].iter().collect();
    let matches = on_disk.len() == unit.raw_lines.len()
        && on_disk
            .iter()
            .zip(&unit.raw_lines)
            .all(|(a, b)| a.as_str() == b.as_str());
    if !matches {
        return Err(format!(
            "content mismatch at {}:{} — file changed on disk since the diff was taken",
            unit.file,
            start + 1
        ));
    }

    let old_len = end - start + 1;
    lines.splice(start..=end, new_lines.iter().cloned());
    let mut out = lines.join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    std::fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(new_lines.len() as i64 - old_len as i64)
}

/// Commit message with app + provenance metadata trailers.
pub fn commit_message(
    unit: &CommentUnit,
    action: Action,
    provenance: &Provenance,
    justification: Option<&str>,
) -> String {
    let verb = match action {
        Action::Keep => "keep",
        Action::Rewrite => "rewrite",
        Action::Delete => "delete",
    };
    let mut msg = format!(
        "review(comments): {verb} comment in {}:{}\n\n",
        unit.file, unit.start_line
    );
    if let Some(j) = justification {
        if !j.trim().is_empty() {
            msg.push_str(&format!("{}\n\n", j.trim()));
        }
    }
    msg.push_str("Reviewed-with: code-review-assistant\n");
    msg.push_str(&format!(
        "Comment-provenance: {}\n",
        provenance.source_str()
    ));
    if let Provenance::Model { coauthor, .. } = provenance {
        if !coauthor.trim().is_empty() {
            msg.push_str(&format!("Co-authored-by: {coauthor}\n"));
        }
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::CommentStyle;

    fn unit() -> CommentUnit {
        CommentUnit {
            file: "src/lib.rs".into(),
            lang: "Rust".into(),
            start_line: 2,
            end_line: 3,
            raw_lines: vec!["    // a".into(), "    // b".into()],
            indent: "    ".into(),
            style: CommentStyle::Line {
                prefix: "//".into(),
            },
            context: String::new(),
            hunk_header: String::new(),
            has_added: true,
        }
    }

    #[test]
    fn final_action_cases() {
        assert_eq!(final_action("same", "same"), Action::Keep);
        assert_eq!(final_action("  ", "same"), Action::Delete);
        assert_eq!(final_action("new", "same"), Action::Rewrite);
    }

    #[test]
    fn provenance_model_vs_edited_vs_human() {
        let chosen = Some(Choice::Candidate(0));
        let m = Some(("claude", "Claude <noreply@anthropic.com>"));
        let p = derive_provenance(&chosen, m, "// x", Some("// x"), "// orig");
        assert_eq!(p.source_str(), "claude");
        let p = derive_provenance(&chosen, m, "// x edited", Some("// x"), "// orig");
        assert_eq!(p.source_str(), "claude+human-edited");
        let p = derive_provenance(&None, None, "// mine", None, "// orig");
        assert_eq!(p.source_str(), "human-authored");
        let p = derive_provenance(&None, None, "// orig", None, "// orig");
        assert_eq!(p.source_str(), "original");
    }

    #[test]
    fn commit_message_has_trailers() {
        let prov = Provenance::Model {
            name: "claude".into(),
            coauthor: "Claude <noreply@anthropic.com>".into(),
            edited: true,
        };
        let msg = commit_message(&unit(), Action::Rewrite, &prov, Some("clearer"));
        assert!(msg.starts_with("review(comments): rewrite comment in src/lib.rs:2"));
        assert!(msg.contains("Reviewed-with: code-review-assistant"));
        assert!(msg.contains("Comment-provenance: claude+human-edited"));
        assert!(msg.contains("Co-authored-by: Claude <noreply@anthropic.com>"));
    }

    #[test]
    fn commit_message_keeps_provenance_without_an_unset_coauthor() {
        let prov = Provenance::Model {
            name: "agy".into(),
            coauthor: String::new(),
            edited: false,
        };
        let msg = commit_message(&unit(), Action::Rewrite, &prov, Some("clearer"));
        assert!(msg.contains("Comment-provenance: agy"), "{msg}");
        assert!(!msg.contains("Co-authored-by:"), "{msg}");
    }

    #[test]
    fn apply_edit_replaces_and_reports_delta() {
        let dir = std::env::temp_dir().join(format!("cra_test_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "fn main() {\n    // a\n    // b\n    x();\n}\n",
        )
        .unwrap();
        let file = ReviewFile {
            path: "src/lib.rs".into(),
            units: vec![],
            line_offset: 0,
            decided: 0,
        };
        let delta = apply_edit(
            dir.to_str().unwrap(),
            &file,
            &unit(),
            &["    // merged".to_string()],
        )
        .unwrap();
        assert_eq!(delta, -1);
        let out = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert_eq!(out, "fn main() {\n    // merged\n    x();\n}\n");
        // mismatch is detected after the file changed
        let err = apply_edit(dir.to_str().unwrap(), &file, &unit(), &[]).unwrap_err();
        assert!(err.contains("mismatch") || err.contains("out of bounds"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
