//! The branch pass: one whole-branch look per model, after the unit walk.
//!
//! The per-unit review judges each change in isolation, which is exactly what
//! it cannot see past: two hunks that contradict each other, a rename applied
//! in some places, a code path left dead, the test a change obviously needs.
//! When the walk finishes, each enabled model gets the branch's full diff and
//! the run of the repository, and reports *cross-cutting* findings — things a
//! verdict on any single unit could not carry. Nothing is edited: findings
//! are recorded, shown for human triage, and dismissable.

use serde::{Deserialize, Serialize};

use crate::models::{self, Evidence};
use crate::settings::ModelSlot;

/// A diff bigger than this goes over truncated. The models can read the
/// repository for the rest; the cap keeps the prompt inside every CLI's
/// comfort zone.
pub const DIFF_BUDGET: usize = 60_000;

#[derive(Clone, Serialize, Deserialize)]
pub struct Finding {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub detail: String,
    /// "high" | "medium" | "low" — model-reported, human-triaged.
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

impl Finding {
    /// Render order and badge colour: high first, unknown severities last.
    pub fn severity_rank(&self) -> u8 {
        match self.severity.trim().to_ascii_lowercase().as_str() {
            "high" => 0,
            "medium" => 1,
            "low" => 2,
            _ => 3,
        }
    }
}

/// The reply shape. `findings` is required — without that, any stray object
/// in a CLI envelope would parse as an empty reply and read as "no findings".
#[derive(Deserialize)]
struct Reply {
    findings: Vec<Finding>,
}

/// Message sent back to the UI thread when one model's branch pass finishes.
pub struct BranchMsg {
    pub seq: u64,
    pub slot_idx: usize,
    pub model: String,
    pub result: Result<Vec<Finding>, String>,
    pub latency_ms: i64,
}

/// Trim the diff to the budget, telling the model when (and how much) is
/// missing so absence of evidence never reads as evidence of absence.
pub fn diff_excerpt(diff: &str) -> String {
    if diff.chars().count() <= DIFF_BUDGET {
        return diff.to_string();
    }
    let head: String = diff.chars().take(DIFF_BUDGET).collect();
    let dropped = diff.chars().count() - DIFF_BUDGET;
    format!(
        "{head}\n\n[diff truncated here — {dropped} more characters. Read the repository and \
`git diff` yourself for the rest before concluding anything about files past this point.]\n"
    )
}

pub fn build_prompt(ref_name: &str, base: &str, n_files: usize, n_units: usize, diff: &str) -> String {
    format!(
        "A unit-by-unit review of branch {ref_name} (against {base}) just finished: {n_units} \
unit(s) across {n_files} file(s), each judged in isolation. Your job now is what that pass \
cannot see — problems that only exist across changes. This is the branch's diff:\n\n{}\n\
Look for cross-cutting findings only; do not re-review individual hunks:\n\
- changes that contradict each other, or a change that breaks an invariant another relies on\n\
- half-applied migrations and renames; code the branch left dead or unreachable\n\
- missing accompaniments: the test, doc, or config a change obviously needs but does not touch\n\
- new logic that duplicates something that already exists elsewhere in the repository\n\
- a mistake or design smell that repeats across the branch and is worth fixing once\n\n\
You are running in the root of the repository, and paths are relative to it. Read whatever \
you need before concluding. Report only what you would actually raise in a review — an empty \
list is a fine answer, and better than a stretched one.\n\n\
Answer with JSON only:\n\
{{\"findings\":[{{\"title\":\"one line\",\"severity\":\"high|medium|low\",\
\"detail\":\"what is wrong, where, and why it matters\",\
\"files\":[\"src/a.rs\",\"src/b.rs\"],\
\"evidence\":[{{\"file\":\"path\",\"lines\":\"12-40\",\"note\":\"what you checked there\"}}]}}]}}\n\
In \"evidence\", list the places you actually read — the reviewer is shown them.",
        diff_excerpt(diff)
    )
}

/// Parse a branch-pass reply out of raw CLI output (envelopes included).
pub fn parse(output: &str) -> Option<Vec<Finding>> {
    models::extract_payload::<Reply>(output).map(|r| r.findings)
}

/// Run one model's branch pass on a background thread, mirroring
/// [`models::spawn_model`] but for the findings payload.
#[allow(clippy::too_many_arguments)]
pub fn spawn_pass(
    seq: u64,
    slot_idx: usize,
    slot: ModelSlot,
    command: String,
    prompt: String,
    repo: String,
    cli_home: String,
    timeout_secs: u64,
    send: impl FnOnce(BranchMsg) + Send + 'static,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        // capture_cli wants a transcript sink; the branch pass has no
        // inspector window, so the useful part of a failure travels through
        // cli_error instead.
        let mut raw = String::new();
        let timeout = std::time::Duration::from_secs(timeout_secs.max(5));
        let result = models::capture_cli(&slot, &command, &prompt, &repo, &cli_home, timeout, &mut raw);
        let (result, latency_ms) = match result {
            Ok((stdout, stderr, latency_ms)) => {
                let parsed = parse(&stdout)
                    .or_else(|| parse(&stderr))
                    .ok_or_else(|| models::cli_error(&slot.name, &stdout, &stderr));
                (parsed, latency_ms)
            }
            Err(e) => (Err(e), 0),
        };
        send(BranchMsg { seq, slot_idx, model: slot.name.clone(), result, latency_ms });
        ctx.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_carries_the_diff_the_task_and_the_schema() {
        let p = build_prompt("feature", "main", 3, 12, "diff --git a/a b/a\n+x\n");
        assert!(p.contains("branch feature"), "{p}");
        assert!(p.contains("against main"));
        assert!(p.contains("diff --git a/a b/a"));
        assert!(p.contains("cross-cutting"));
        assert!(p.contains("\"findings\""));
        assert!(p.contains("an empty list is a fine answer"), "{p}");
        assert!(p.contains("root of the repository"));
    }

    #[test]
    fn an_oversized_diff_is_truncated_with_a_visible_seam() {
        let big = "x".repeat(DIFF_BUDGET + 500);
        let cut = diff_excerpt(&big);
        assert!(cut.len() < big.len());
        assert!(cut.contains("diff truncated here — 500 more characters"), "{}", &cut[DIFF_BUDGET..]);
        let small = "just a diff";
        assert_eq!(diff_excerpt(small), small);
    }

    #[test]
    fn findings_parse_from_noisy_output_and_envelopes() {
        let noisy = "Here you go:\n```json\n{\"findings\":[\
{\"title\":\"rename half applied\",\"severity\":\"high\",\
\"detail\":\"retry_limit renamed in config.rs but not in worker.rs\",\
\"files\":[\"src/config.rs\",\"src/worker.rs\"],\
\"evidence\":[{\"file\":\"src/worker.rs\",\"lines\":\"40-55\",\"note\":\"still reads old key\"}]}]}\n```";
        let f = parse(noisy).expect("parse");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].title, "rename half applied");
        assert_eq!(f[0].severity_rank(), 0);
        assert_eq!(f[0].files.len(), 2);
        assert_eq!(f[0].evidence.len(), 1);

        // A CLI envelope with the reply as an escaped string inside it.
        let envelope = "{\"conversation_id\":\"abc\",\"response\":\"{\\\"findings\\\":[]}\"}";
        assert_eq!(parse(envelope).expect("envelope").len(), 0);

        // No findings key anywhere: parsing must fail, not read as "clean".
        assert!(parse("{\"ok\":true}").is_none());
    }

    #[test]
    fn severity_ranks_order_and_tolerate_nonsense() {
        let sev = |s: &str| Finding { severity: s.into(), ..blank() };
        assert!(sev("high").severity_rank() < sev("medium").severity_rank());
        assert!(sev("medium").severity_rank() < sev("low").severity_rank());
        assert!(sev("low").severity_rank() < sev("catastrophic?!").severity_rank());
        assert_eq!(sev(" HIGH ").severity_rank(), 0);
    }

    fn blank() -> Finding {
        Finding {
            title: String::new(),
            detail: String::new(),
            severity: String::new(),
            files: Vec::new(),
            evidence: Vec::new(),
        }
    }
}
