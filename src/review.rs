//! Review session state: the plan (files × comment units), edit application
//! to the working tree, provenance tracking, and commit message assembly.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::models::Action;
use crate::units::{ReviewUnit, UnitKind};

#[derive(Clone, PartialEq)]
pub enum RefKind {
    Branch,
    Pr(u64),
    WorkingTree,
    /// Re-judging comments already decided once, to measure how consistent the
    /// reviewer is with their own earlier self. Nothing is written to disk.
    Recheck,
}

impl RefKind {
    pub fn label(&self) -> String {
        match self {
            RefKind::Branch => "branch".into(),
            RefKind::Pr(n) => format!("PR #{n}"),
            RefKind::WorkingTree => "working tree".into(),
            RefKind::Recheck => "re-check".into(),
        }
    }
}

pub struct ReviewFile {
    pub path: String,
    pub units: Vec<ReviewUnit>,
    /// Every edit applied to this file so far, as (original start line,
    /// line-count delta). Kept per edit rather than as one running total so
    /// the walk may visit units in any order — riskiest first included — and
    /// still land each edit exactly where it belongs: only edits *above* a
    /// unit shift it.
    pub edits: Vec<(u32, i64)>,
    pub decided: usize,
}

impl ReviewFile {
    /// How far the given (original) line has drifted on disk: the sum of the
    /// deltas of every applied edit that started above it. Units are
    /// disjoint, so comparing original start lines is exact.
    pub fn offset_for(&self, start_line: u32) -> i64 {
        self.edits.iter().filter(|(at, _)| *at < start_line).map(|(_, d)| d).sum()
    }
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
    /// A re-check re-judges history, so it must not touch the working tree:
    /// the files may have moved on, and the point is the judgement, not the
    /// edit.
    pub fn is_recheck(&self) -> bool {
        self.ref_kind == RefKind::Recheck
    }

    pub fn total_units(&self) -> usize {
        self.files.iter().map(|f| f.units.len()).sum()
    }

    pub fn current(&self) -> Option<(&ReviewFile, &ReviewUnit)> {
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

/// The order to show candidates in, as display position -> slot index.
///
/// Deterministic in `seed` so the cards do not reshuffle on every repaint, and
/// so re-reviewing the same comment presents it the same way. A plain
/// Fisher-Yates over a small LCG: this only has to be unbiased across
/// comments, not cryptographic.
pub fn blind_order(seed: u64, n: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    for i in (1..n).rev() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let j = (state >> 33) as usize % (i + 1);
        order.swap(i, j);
    }
    order
}

/// Stable identity for a comment, used to seed its candidate order.
pub fn unit_seed(file: &str, start_line: u32) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in file.as_bytes().iter().chain(&start_line.to_le_bytes()) {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// What the human picked (before free-form edits are considered).
#[derive(Clone, PartialEq, Debug)]
pub enum Choice {
    /// Candidate from model slot i (index into the session's model list).
    Candidate(usize),
    /// Explicit "keep the original text".
    KeepOriginal,
    /// Explicit "delete this comment".
    Delete,
}

/// Provenance of the final text, derived at save time.
#[derive(Clone)]
pub enum Provenance {
    Unchanged,
    Model { name: String, coauthor: String, edited: bool },
    Human,
}

impl Provenance {
    pub fn source_str(&self) -> String {
        match self {
            Provenance::Unchanged => "original".into(),
            Provenance::Model { name, edited: false, .. } => name.clone(),
            Provenance::Model { name, edited: true, .. } => format!("{name}+human-edited"),
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
            Provenance::Model { name: name.to_string(), coauthor: coauthor.to_string(), edited }
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
    unit: &ReviewUnit,
    new_lines: &[String],
) -> Result<i64, String> {
    let offset = file.offset_for(unit.start_line());
    let start = (unit.start_line() as i64 - 1 + offset).max(0) as usize;
    splice_lines(repo, unit.file(), start, unit.raw_lines(), new_lines)
}

/// Swap `expect` for `replace` at `start0` (0-based), refusing to touch the
/// file unless it still contains exactly `expect` there. The same primitive
/// applies an edit and — with the arguments swapped — reverts one that a
/// failed validation should not leave behind.
pub fn splice_lines(
    repo: &str,
    file_rel: &str,
    start0: usize,
    expect: &[String],
    replace: &[String],
) -> Result<i64, String> {
    let path = Path::new(repo).join(file_rel);
    let content = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let had_trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();

    let end = start0 + expect.len().saturating_sub(1); // inclusive
    if expect.is_empty() || end >= lines.len() {
        return Err(format!(
            "line range {}..{} out of bounds for {file_rel} ({} lines) — file changed on disk?",
            start0 + 1,
            end + 1,
            lines.len()
        ));
    }
    // Sanity check: the file should still contain what we think it does.
    let matches = lines[start0..=end].iter().zip(expect).all(|(a, b)| a == b);
    if !matches {
        return Err(format!(
            "content mismatch at {file_rel}:{} — file changed on disk since the diff was taken",
            start0 + 1
        ));
    }

    lines.splice(start0..=end, replace.iter().cloned());
    let mut out = lines.join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    std::fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(replace.len() as i64 - expect.len() as i64)
}

/// Run the configured validation command in the repository, whitespace-
/// tokenized like a model template — no shell. Ok means exit 0 in time;
/// anything else returns the tail of what the command printed, which is
/// where compilers and test runners put the part worth reading.
pub fn run_check(repo: &str, command: &str, timeout: Duration) -> Result<(), String> {
    let argv: Vec<&str> = command.split_whitespace().collect();
    let Some(program) = argv.first() else { return Err("empty check command".into()) };
    let started = Instant::now();
    let mut child = crate::gitio::hidden_command(program)
        .args(&argv[1..])
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn `{program}`: {e}"))?;
    let deadline = started + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("check timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(stderr.trim());
    }
    let tail: String = {
        let chars: Vec<char> = text.chars().collect();
        chars[chars.len().saturating_sub(600)..].iter().collect()
    };
    Err(format!("`{command}` failed: {}", tail.trim()))
}

/// One decision that has changed the working tree but is not committed yet.
/// Accumulated across `Save and Continue` calls on the same file, so that a
/// later `Commit and Continue` can document every one of them instead of just
/// the last, even though `git commit` only ever sees the file's final state.
#[derive(Clone)]
pub struct PendingDecision {
    pub file: String,
    pub line: u32,
    pub kind: UnitKind,
    pub action: Action,
    pub provenance: Provenance,
    pub justification: Option<String>,
    /// `(model variant, effort)` from the chosen candidate's slot, when the
    /// decision came from a model. Either half may be empty — the CLI default.
    pub model_info: Option<(String, String)>,
}

/// The verb for a commit subject, phrased for what was actually reviewed:
/// comments are rewritten, code is revised.
fn action_verb(kind: UnitKind, action: Action) -> &'static str {
    match (kind, action) {
        (_, Action::Keep) => "keep",
        (UnitKind::Comment, Action::Rewrite) => "rewrite",
        (UnitKind::Code, Action::Rewrite) => "revise",
        (_, Action::Delete) => "delete",
        (_, Action::Flag) => "flag",
    }
}

/// `review(comments)` / `review(code)` / plain `review` for a mixed batch.
fn subject_scope(kinds: &[UnitKind]) -> &'static str {
    let any_comment = kinds.contains(&UnitKind::Comment);
    let any_code = kinds.contains(&UnitKind::Code);
    match (any_comment, any_code) {
        (true, false) => "review(comments)",
        (false, true) => "review(code)",
        _ => "review",
    }
}

fn push_model_trailers(msg: &mut String, provenance: &Provenance, model_info: Option<(&str, &str)>) {
    if let Provenance::Model { coauthor, .. } = provenance {
        if !coauthor.trim().is_empty() {
            msg.push_str(&format!("Co-authored-by: {coauthor}\n"));
        }
    }
    if let Some((model, effort)) = model_info {
        if !model.trim().is_empty() {
            msg.push_str(&format!("Model: {}\n", model.trim()));
        }
        if !effort.trim().is_empty() {
            msg.push_str(&format!("Effort: {}\n", effort.trim()));
        }
    }
}

/// Commit message covering every pending decision on the file being
/// committed — the fix for a batch of `Save and Continue`s losing all but the
/// last decision's message once `Commit and Continue` finally runs `git
/// commit`. `entries` must be non-empty and share the same `file`.
pub fn commit_message_batch(entries: &[PendingDecision]) -> String {
    assert!(!entries.is_empty(), "commit_message_batch needs at least one decision");
    let kinds: Vec<UnitKind> = entries.iter().map(|e| e.kind).collect();
    if entries.len() == 1 {
        let e = &entries[0];
        let what = match e.kind {
            UnitKind::Comment => " comment in",
            UnitKind::Code => " code in",
        };
        let mut msg = format!(
            "{}: {}{what} {}:{}\n\n",
            subject_scope(&kinds),
            action_verb(e.kind, e.action),
            e.file,
            e.line
        );
        if let Some(j) = &e.justification {
            if !j.trim().is_empty() {
                msg.push_str(&format!("{}\n\n", j.trim()));
            }
        }
        msg.push_str("Reviewed-with: code-review-assistant\n");
        let trailer = match e.kind {
            UnitKind::Comment => "Comment-provenance",
            UnitKind::Code => "Change-provenance",
        };
        msg.push_str(&format!("{trailer}: {}\n", e.provenance.source_str()));
        push_model_trailers(&mut msg, &e.provenance, e.model_info.as_ref().map(|(m, ef)| (m.as_str(), ef.as_str())));
        return msg;
    }

    let file = &entries[0].file;
    let noun = match subject_scope(&kinds) {
        "review(comments)" => "comment decisions",
        _ => "decisions",
    };
    let mut msg =
        format!("{}: {} {noun} in {file}\n\n", subject_scope(&kinds), entries.len());
    for e in entries {
        msg.push_str(&format!("- {} {file}:{}", action_verb(e.kind, e.action), e.line));
        let mut detail: Vec<String> = vec![e.provenance.source_str()];
        if let Some((model, _)) = &e.model_info {
            if !model.trim().is_empty() {
                detail.push(model.trim().to_string());
            }
        }
        msg.push_str(&format!(" ({})", detail.join(", ")));
        if let Some(j) = &e.justification {
            if !j.trim().is_empty() {
                msg.push_str(&format!(" — {}", j.trim()));
            }
        }
        msg.push('\n');
    }
    msg.push('\n');
    msg.push_str("Reviewed-with: code-review-assistant\n");

    // Trailers are deduplicated across the batch: several decisions may share
    // the same model, and repeating its Co-authored-by would just be noise
    // (and some git hosts collapse it anyway).
    let mut coauthors: Vec<&str> = Vec::new();
    let mut model_infos: Vec<(&str, &str)> = Vec::new();
    for e in entries {
        if let Provenance::Model { coauthor, .. } = &e.provenance {
            if !coauthor.trim().is_empty() && !coauthors.contains(&coauthor.as_str()) {
                coauthors.push(coauthor);
            }
        }
        if let Some((model, effort)) = &e.model_info {
            let pair = (model.as_str(), effort.as_str());
            if !model.trim().is_empty() && !model_infos.contains(&pair) {
                model_infos.push(pair);
            }
        }
    }
    for coauthor in &coauthors {
        msg.push_str(&format!("Co-authored-by: {coauthor}\n"));
    }
    for (model, effort) in &model_infos {
        msg.push_str(&format!("Model: {model}\n"));
        if !effort.trim().is_empty() {
            msg.push_str(&format!("Effort: {}\n", effort.trim()));
        }
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::{CommentStyle, CommentUnit};

    fn unit() -> CommentUnit {
        CommentUnit {
            file: "src/lib.rs".into(),
            lang: "Rust".into(),
            start_line: 2,
            end_line: 3,
            raw_lines: vec!["    // a".into(), "    // b".into()],
            indent: "    ".into(),
            style: CommentStyle::Line { prefix: "//".into() },
            context: String::new(),
            hunk_header: String::new(),
            has_added: true,
        }
    }

    #[test]
    fn blind_order_is_a_permutation_and_stable_per_comment() {
        for n in 1..=6 {
            let order = blind_order(unit_seed("src/lib.rs", 12), n);
            let mut sorted = order.clone();
            sorted.sort_unstable();
            // Every slot appears exactly once, or a candidate would be shown
            // twice or not at all.
            assert_eq!(sorted, (0..n).collect::<Vec<_>>(), "n={n}");
            // Stable, so the cards do not reshuffle on every repaint.
            assert_eq!(order, blind_order(unit_seed("src/lib.rs", 12), n));
        }
    }

    #[test]
    fn blind_order_varies_between_comments() {
        // Three slots have six orderings; over many comments a fixed order
        // would put the same model first every time and reintroduce the bias
        // blinding exists to remove.
        let first_slots: std::collections::HashSet<usize> = (0..60)
            .map(|line| blind_order(unit_seed("src/lib.rs", line), 3)[0])
            .collect();
        assert_eq!(first_slots.len(), 3, "every slot should lead sometimes: {first_slots:?}");
    }

    #[test]
    fn blind_order_handles_a_single_candidate() {
        assert_eq!(blind_order(unit_seed("a.rs", 1), 1), vec![0]);
        assert!(blind_order(unit_seed("a.rs", 1), 0).is_empty());
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

    fn pending(file: &str, line: u32, action: Action, justification: &str) -> PendingDecision {
        PendingDecision {
            file: file.into(),
            line,
            kind: UnitKind::Comment,
            action,
            provenance: Provenance::Model {
                name: "claude".into(),
                coauthor: "Claude <noreply@anthropic.com>".into(),
                edited: false,
            },
            justification: Some(justification.into()),
            model_info: Some(("claude-sonnet-5".into(), "low".into())),
        }
    }

    #[test]
    fn a_single_pending_decision_reads_like_the_old_single_commit_message() {
        let mut e = pending("src/lib.rs", 2, Action::Rewrite, "clearer");
        e.provenance = Provenance::Model {
            name: "claude".into(),
            coauthor: "Claude <noreply@anthropic.com>".into(),
            edited: true,
        };
        e.model_info = None;
        let msg = commit_message_batch(&[e]);
        assert!(msg.starts_with("review(comments): rewrite comment in src/lib.rs:2"));
        assert!(msg.contains("clearer"));
        assert!(msg.contains("Reviewed-with: code-review-assistant"));
        assert!(msg.contains("Comment-provenance: claude+human-edited"));
        assert!(msg.contains("Co-authored-by: Claude <noreply@anthropic.com>"));
    }

    #[test]
    fn commit_message_keeps_provenance_without_an_unset_coauthor() {
        let entry = PendingDecision {
            file: "src/lib.rs".into(),
            line: 2,
            action: Action::Rewrite,
            provenance: Provenance::Model {
                name: "agy".into(),
                coauthor: String::new(),
                edited: false,
            },
            justification: Some("clearer".into()),
            model_info: Some(("gemini-3.7-flash-low".into(), String::new())),
        };
        let msg = commit_message_batch(&[entry]);
        assert!(msg.contains("Comment-provenance: agy"), "{msg}");
        assert!(msg.contains("Model: gemini-3.7-flash-low"), "{msg}");
        assert!(!msg.contains("Co-authored-by:"), "{msg}");
    }

    #[test]
    fn commit_message_records_model_and_effort() {
        let entries = vec![pending("src/lib.rs", 2, Action::Rewrite, "clearer")];
        let msg = commit_message_batch(&entries);
        assert!(msg.contains("Model: claude-sonnet-5"));
        assert!(msg.contains("Effort: low"));
    }

    #[test]
    fn commit_message_omits_empty_model_and_effort() {
        let entries = vec![PendingDecision {
            file: "src/lib.rs".into(),
            line: 2,
            kind: UnitKind::Comment,
            action: Action::Delete,
            provenance: Provenance::Human,
            justification: None,
            model_info: Some(("".into(), "".into())),
        }];
        let msg = commit_message_batch(&entries);
        assert!(!msg.contains("Model:"));
        assert!(!msg.contains("Effort:"));
    }

    #[test]
    fn batch_documents_every_pending_decision() {
        let entries = vec![
            pending("src/lib.rs", 2, Action::Delete, "restates the code"),
            pending("src/lib.rs", 9, Action::Rewrite, "clarifies the retry limit"),
        ];
        let msg = commit_message_batch(&entries);
        assert!(msg.starts_with("review(comments): 2 comment decisions in src/lib.rs"));
        assert!(msg.contains("delete src/lib.rs:2"), "{msg}");
        assert!(msg.contains("restates the code"), "{msg}");
        assert!(msg.contains("rewrite src/lib.rs:9"), "{msg}");
        assert!(msg.contains("clarifies the retry limit"), "{msg}");
        // Trailers are deduplicated, not repeated once per decision.
        assert_eq!(msg.matches("Co-authored-by:").count(), 1, "{msg}");
        assert_eq!(msg.matches("Model:").count(), 1, "{msg}");
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
        let file = ReviewFile { path: "src/lib.rs".into(), units: vec![], edits: Vec::new(), decided: 0 };
        let wrapped = ReviewUnit::Comment(unit());
        let delta = apply_edit(
            dir.to_str().unwrap(),
            &file,
            &wrapped,
            &["    // merged".to_string()],
        )
        .unwrap();
        assert_eq!(delta, -1);
        let out = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert_eq!(out, "fn main() {\n    // merged\n    x();\n}\n");
        // mismatch is detected after the file changed
        let err = apply_edit(dir.to_str().unwrap(), &file, &wrapped, &[]).unwrap_err();
        assert!(err.contains("mismatch") || err.contains("out of bounds"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn offsets_only_count_edits_above_the_unit() {
        let mut f = ReviewFile { path: "a".into(), units: vec![], edits: Vec::new(), decided: 0 };
        f.edits.push((50, 3));
        assert_eq!(f.offset_for(10), 0, "an edit below must not shift a unit above it");
        assert_eq!(f.offset_for(60), 3);
        f.edits.push((5, -2));
        assert_eq!(f.offset_for(10), -2);
        assert_eq!(f.offset_for(60), 1, "deltas above accumulate");
        assert_eq!(f.offset_for(5), 0, "an edit at the unit's own line is that unit's own");
    }

    #[test]
    fn splice_lines_reverts_what_it_applied() {
        let dir = crate::testkit::TempDir::new("revert");
        std::fs::write(dir.path().join("a.rs"), "one\ntwo\nthree\n").unwrap();
        let repo = dir.path().to_string_lossy().to_string();
        let old = vec!["two".to_string()];
        let new = vec!["2a".to_string(), "2b".to_string()];
        assert_eq!(splice_lines(&repo, "a.rs", 1, &old, &new).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "one\n2a\n2b\nthree\n");
        // The exact inverse call restores the file byte for byte.
        assert_eq!(splice_lines(&repo, "a.rs", 1, &new, &old).unwrap(), -1);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "one\ntwo\nthree\n");
    }

    #[test]
    fn a_single_code_decision_reads_as_a_code_review_commit() {
        let mut e = pending("src/lib.rs", 42, Action::Rewrite, "off by one");
        e.kind = UnitKind::Code;
        let msg = commit_message_batch(&[e]);
        assert!(msg.starts_with("review(code): revise code in src/lib.rs:42"), "{msg}");
        assert!(msg.contains("Change-provenance: claude"), "{msg}");
        assert!(!msg.contains("Comment-provenance"), "{msg}");
    }

    #[test]
    fn a_mixed_batch_says_review_and_uses_each_kinds_verb() {
        let mut code = pending("src/lib.rs", 9, Action::Rewrite, "simpler");
        code.kind = UnitKind::Code;
        let msg = commit_message_batch(&[
            pending("src/lib.rs", 2, Action::Delete, "restates the code"),
            code,
        ]);
        assert!(msg.starts_with("review: 2 decisions in src/lib.rs"), "{msg}");
        assert!(msg.contains("delete src/lib.rs:2"), "{msg}");
        assert!(msg.contains("revise src/lib.rs:9"), "{msg}");
    }

    #[test]
    fn run_check_passes_and_fails_with_the_commands_own_words() {
        use crate::testkit::{FakeCli, FakeCliSpec, TempDir};
        let dir = TempDir::new("check");
        let repo = dir.path().to_string_lossy().to_string();
        let timeout = Duration::from_secs(10);

        let ok = FakeCli::new(&dir, "check-ok", FakeCliSpec { reply: "all good", ..Default::default() });
        assert!(run_check(&repo, &ok.command(), timeout).is_ok());

        let bad = FakeCli::new(
            &dir,
            "check-bad",
            FakeCliSpec { reply: "error[E0308]: mismatched types", exit_code: 2, ..Default::default() },
        );
        let err = run_check(&repo, &bad.command(), timeout).unwrap_err();
        // The command's own output is the error the reviewer sees.
        assert!(err.contains("mismatched types"), "{err}");
        assert!(err.contains("failed"), "{err}");

        assert!(run_check(&repo, "definitely-not-a-real-binary-xyz", timeout)
            .unwrap_err()
            .contains("spawn"));
        assert!(run_check(&repo, "   ", timeout).unwrap_err().contains("empty"));
    }
}
