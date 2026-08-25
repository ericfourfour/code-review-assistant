//! Evaluating the models against the reviewer.
//!
//! There is no ground truth for whether a comment should be kept, rewritten,
//! or deleted — the reviewer's judgement *is* the label. So nothing here
//! reports "accuracy": it reports agreement with a particular human, bounded
//! by how consistently that human agrees with themselves.
//!
//! Every review already produces the data. A decision row stores the unit that
//! was judged and the verdict; the suggestion rows store what each model
//! proposed for that same unit. [`Corpus`] is that pulled out into a file so a
//! run can be repeated against a different model, effort level, or prompt.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::models::{self, Action};
use crate::settings::Settings;
use crate::units::ReviewUnit;

/// One labelled example: a unit (comment or code), and what the reviewer
/// decided about it.
#[derive(Clone, Serialize, Deserialize)]
pub struct CorpusEntry {
    pub unit: ReviewUnit,
    /// keep / rewrite / delete, as the human left it.
    pub action: String,
    /// The text the human settled on (empty for a deletion).
    pub final_text: String,
    /// Which candidate the text came from: a model name, `original`,
    /// `human-authored`, or `<model>+human-edited`.
    pub source: String,
    /// Whether model identities were hidden when this was decided. An
    /// unblinded label is still usable, but it is weaker evidence.
    pub blinded: bool,
    /// The human reworded whatever they started from before saving. Useful as
    /// a "close, but not right" signal when scoring.
    #[serde(default)]
    pub human_edited: bool,
    /// The repository the comment was judged in. A replay runs the models
    /// there, so they can read the same code the reviewer could see; entries
    /// exported before this field existed carry none, and replay in that case
    /// falls back to the hunk alone.
    #[serde(default)]
    pub repo: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Corpus {
    pub entries: Vec<CorpusEntry>,
}

impl Corpus {
    /// Pull the most recent labelled decisions out of the database.
    pub fn from_db(db: &Db, limit: usize) -> Corpus {
        Self::from_rows(db.corpus(limit))
    }

    /// Pull labels for a re-check in one selected repository. Each model call
    /// will run in that checkout, so every entry must have been judged there.
    pub fn from_db_for_repo(db: &Db, repo: &str, limit: usize) -> Corpus {
        Self::from_rows(db.corpus_for_repo(repo, limit))
    }

    fn from_rows(rows: Vec<crate::db::CorpusRow>) -> Corpus {
        let entries = rows
            .into_iter()
            .filter_map(|row| {
                let unit: ReviewUnit = serde_json::from_str(&row.unit_json).ok()?;
                Some(CorpusEntry {
                    unit,
                    action: row.action,
                    final_text: row.final_text,
                    source: row.source,
                    blinded: row.blinded,
                    human_edited: row.human_edited,
                    repo: row.repo,
                })
            })
            .collect();
        Corpus { entries }
    }

    pub fn load(path: &std::path::Path) -> Result<Corpus, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let text =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize corpus: {e}"))?;
        std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
    }
}

/// What one model said about one corpus entry on a replay.
#[derive(Clone, Serialize, Deserialize)]
pub struct ReplayAnswer {
    pub model: String,
    /// Identifies which entry this answers, by position in the corpus.
    pub entry: usize,
    pub action: Option<String>,
    pub comment: String,
    pub latency_ms: i64,
    pub error: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ReplayResults {
    pub answers: Vec<ReplayAnswer>,
}

/// Run every enabled model over every corpus entry. Sequential on purpose:
/// this is a background measurement, and running the CLIs flat out competes
/// with whatever the machine is actually doing.
pub fn replay(
    settings: &Settings,
    corpus: &Corpus,
    mut progress: impl FnMut(usize, usize, &str),
) -> ReplayResults {
    let timeout = Duration::from_secs(settings.model_timeout_secs.max(5));
    let slots = settings.enabled_models();
    let total = corpus.entries.len() * slots.len();
    let mut answers = Vec::with_capacity(total);
    let mut done = 0;

    for (idx, entry) in corpus.entries.iter().enumerate() {
        let prompt = entry.unit.build_prompt();
        // A corpus outlives the checkout it came from: the repository may have
        // been moved or deleted since, and running the models in a directory
        // that is no longer there would fail every entry. Falling back to no
        // working directory costs them the ability to browse, which the prompt
        // has already promised — so say which entries lost it.
        let repo = match entry.repo.trim() {
            "" => String::new(),
            path if std::path::Path::new(path).is_dir() => path.to_string(),
            path => {
                progress(done, total, &format!("(repo gone: {path})"));
                String::new()
            }
        };
        // A CLI whose permissions come from a config file gets the home this
        // app manages, pointed at the repository this entry was judged in.
        let cli_home = crate::agycli::configure(&repo).map(|h| h.to_string_lossy().to_string());
        let cli_home = cli_home.unwrap_or_default();
        for (_, slot) in &slots {
            // A replay is one-shot per entry, so no session id is threaded
            // through; `{session}` would be meaningless without a follow-up.
            let command = slot
                .command
                .replace("{session}", &uuid::Uuid::new_v4().to_string());
            let (result, _raw) =
                models::run_for_eval(slot, &command, &prompt, &repo, &cli_home, timeout);
            let answer = match result {
                Ok(s) => ReplayAnswer {
                    model: slot.name.clone(),
                    entry: idx,
                    action: Some(s.action.as_str().to_string()),
                    comment: s.comment,
                    latency_ms: s.latency_ms,
                    error: None,
                },
                Err(e) => ReplayAnswer {
                    model: slot.name.clone(),
                    entry: idx,
                    action: None,
                    comment: String::new(),
                    latency_ms: 0,
                    error: Some(e),
                },
            };
            done += 1;
            progress(done, total, &slot.name);
            answers.push(answer);
        }
    }
    ReplayResults { answers }
}

/// Per-model agreement with the reviewer.
#[derive(Default, Clone)]
pub struct ModelScore {
    pub judged: usize,
    pub errors: usize,
    /// Same keep/rewrite/delete verdict as the human.
    pub action_agreed: usize,
    /// The human ended up using this model's text.
    pub accepted: usize,
    /// Accepted, then edited before saving — close, but not right.
    pub accepted_edited: usize,
    pub total_latency_ms: i64,
}

impl ModelScore {
    fn pct(n: usize, d: usize) -> f64 {
        if d == 0 {
            0.0
        } else {
            100.0 * n as f64 / d as f64
        }
    }

    pub fn agreement_pct(&self) -> f64 {
        Self::pct(self.action_agreed, self.judged.saturating_sub(self.errors))
    }

    pub fn acceptance_pct(&self) -> f64 {
        Self::pct(self.accepted, self.judged)
    }

    pub fn mean_latency_ms(&self) -> i64 {
        let answered = self.judged.saturating_sub(self.errors);
        if answered == 0 {
            0
        } else {
            self.total_latency_ms / answered as i64
        }
    }
}

/// The full picture: per-model scores plus the reviewer's own consistency.
pub struct Report {
    pub models: BTreeMap<String, ModelScore>,
    /// Comments judged more than once, and how often the verdict matched.
    pub repeat_total: usize,
    pub repeat_agreed: usize,
    /// How many of the scored decisions were made with names hidden.
    pub blinded: usize,
    pub decisions: usize,
}

impl Report {
    /// Build from the review history: what models proposed, next to what the
    /// human did about it.
    pub fn from_db(db: &Db) -> Report {
        let mut models: BTreeMap<String, ModelScore> = BTreeMap::new();
        let mut blinded = 0;
        let mut decisions = 0;
        for row in db.agreement_rows() {
            let score = models.entry(row.model.clone()).or_default();
            score.judged += 1;
            decisions += 1;
            if row.blinded {
                blinded += 1;
            }
            if row.error.is_some() {
                score.errors += 1;
                continue;
            }
            score.total_latency_ms += row.latency_ms;
            if row.model_action.as_deref() == Some(row.human_action.as_str()) {
                score.action_agreed += 1;
            }
            // `source` is the model's name, or "<name>+human-edited".
            if row.human_source == row.model {
                score.accepted += 1;
            } else if row.human_source.starts_with(&format!("{}+", row.model)) {
                score.accepted += 1;
                score.accepted_edited += 1;
            }
        }

        let repeats = db.repeated_decisions();
        let mut repeat_total = 0;
        let mut repeat_agreed = 0;
        for (_, actions) in &repeats {
            for pair in actions.windows(2) {
                repeat_total += 1;
                if pair[0] == pair[1] {
                    repeat_agreed += 1;
                }
            }
        }

        Report {
            models,
            repeat_total,
            repeat_agreed,
            blinded,
            decisions,
        }
    }

    /// Build from a replay against a labelled corpus.
    pub fn from_replay(corpus: &Corpus, results: &ReplayResults) -> Report {
        let mut models: BTreeMap<String, ModelScore> = BTreeMap::new();
        let mut blinded = 0;
        for answer in &results.answers {
            let Some(entry) = corpus.entries.get(answer.entry) else {
                continue;
            };
            let score = models.entry(answer.model.clone()).or_default();
            score.judged += 1;
            if entry.blinded {
                blinded += 1;
            }
            if answer.error.is_some() {
                score.errors += 1;
                continue;
            }
            score.total_latency_ms += answer.latency_ms;
            if answer.action.as_deref() == Some(entry.action.as_str()) {
                score.action_agreed += 1;
            }
            // On a replay there is no human in the loop to accept anything, so
            // count an exact text match as the strongest available signal.
            if entry.action == Action::Rewrite.as_str()
                && !answer.comment.trim().is_empty()
                && entry.final_text.contains(answer.comment.trim())
            {
                score.accepted += 1;
                if entry.human_edited {
                    score.accepted_edited += 1;
                }
            }
        }
        Report {
            models,
            repeat_total: 0,
            repeat_agreed: 0,
            blinded,
            decisions: corpus.entries.len(),
        }
    }

    pub fn self_agreement_pct(&self) -> Option<f64> {
        if self.repeat_total == 0 {
            None
        } else {
            Some(100.0 * self.repeat_agreed as f64 / self.repeat_total as f64)
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{:<12} {:>7} {:>7} {:>9} {:>10} {:>8} {:>9}\n",
            "model", "judged", "errors", "agreed", "accepted", "edited", "mean ms"
        ));
        out.push_str(&"-".repeat(68));
        out.push('\n');
        for (name, s) in &self.models {
            out.push_str(&format!(
                "{:<12} {:>7} {:>7} {:>8.0}% {:>9.0}% {:>8} {:>9}\n",
                name,
                s.judged,
                s.errors,
                s.agreement_pct(),
                s.acceptance_pct(),
                s.accepted_edited,
                s.mean_latency_ms(),
            ));
        }
        out.push('\n');
        match self.self_agreement_pct() {
            Some(pct) => out.push_str(&format!(
                "Reviewer self-agreement: {pct:.0}% over {} repeated comment(s).\n\
                 No model can be meaningfully scored above this.\n",
                self.repeat_total
            )),
            None => out.push_str(
                "Reviewer self-agreement: unknown — no comment has been judged twice.\n\
                 Without it there is no noise floor, so treat differences between\n\
                 models of a few points as indistinguishable.\n",
            ),
        }
        if self.decisions > 0 && self.blinded < self.decisions {
            out.push_str(&format!(
                "\nWarning: {} of {} scored judgements were made with model names\n\
                 visible, so those labels may reflect existing preference.\n",
                self.decisions - self.blinded,
                self.decisions
            ));
        }
        out.push_str(
            "\nAgreement is with this reviewer, not with any ground truth:\n\
             whether a comment earns its place is a judgement call.\n",
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::{CommentStyle, CommentUnit};

    fn unit(file: &str, line: u32) -> ReviewUnit {
        ReviewUnit::Comment(CommentUnit {
            file: file.into(),
            lang: "Rust".into(),
            start_line: line,
            end_line: line,
            raw_lines: vec!["    // a".into()],
            indent: "    ".into(),
            style: CommentStyle::Line {
                prefix: "//".into(),
            },
            context: String::new(),
            hunk_header: String::new(),
            has_added: true,
        })
    }

    fn entry(action: &str, final_text: &str, blinded: bool) -> CorpusEntry {
        CorpusEntry {
            unit: unit("src/lib.rs", 2),
            action: action.into(),
            final_text: final_text.into(),
            source: "human-authored".into(),
            blinded,
            human_edited: false,
            repo: String::new(),
        }
    }

    fn log_label(db: &Db, session_id: i64, unit: &CommentUnit, action: &str, source: &str) {
        let original = unit.raw_lines.join("\n");
        let unit_json = serde_json::to_string(unit).unwrap();
        db.log_decision(&crate::db::DecisionRecord {
            session_id,
            file: &unit.file,
            line_start: unit.start_line,
            line_end: unit.end_line,
            original: &original,
            action,
            final_text: &original,
            source,
            human_edited: false,
            committed: false,
            commit_sha: None,
            justification: None,
            unit_json: Some(&unit_json),
            blinded: true,
        });
    }

    #[test]
    fn a_corpus_entry_remembers_the_repository_it_was_judged_in() {
        let dir = crate::testkit::TempDir::new("corpus_repo");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let session = db.new_session("C:/work/widgets", "branch", "feature", "main");
        let unit = unit("src/lib.rs", 2);
        db.log_decision(&crate::db::DecisionRecord {
            session_id: session,
            file: "src/lib.rs",
            line_start: 2,
            line_end: 2,
            original: "    // a",
            action: "keep",
            final_text: "    // a",
            source: "original",
            human_edited: false,
            committed: false,
            commit_sha: None,
            justification: None,
            unit_json: Some(&serde_json::to_string(&unit).unwrap()),
            blinded: true,
        });

        // Without the repository a replay cannot put the models back where the
        // reviewer was standing, and it would measure them on strictly less
        // than the human had.
        let corpus = Corpus::from_db(&db, 10);
        assert_eq!(corpus.entries.len(), 1);
        assert_eq!(corpus.entries[0].repo, "C:/work/widgets");
    }

    #[test]
    fn a_corpus_round_trips_through_a_file() {
        let dir = crate::testkit::TempDir::new("corpus");
        let path = dir.path().join("corpus.json");
        let corpus = Corpus {
            entries: vec![
                entry("rewrite", "    // better", true),
                entry("keep", "    // a", false),
            ],
        };
        corpus.save(&path).unwrap();

        let loaded = Corpus::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.entries[0].action, "rewrite");
        assert!(
            !loaded.entries[0].unit.is_code(),
            "the kind must survive the round trip"
        );
        assert!(loaded.entries[0].blinded);
        assert!(!loaded.entries[1].blinded);
        // The unit has to survive intact, or a replay is not asking the same
        // question the human answered.
        assert_eq!(
            loaded.entries[0].unit.raw_lines(),
            vec!["    // a".to_string()]
        );
    }

    #[test]
    fn scoring_a_replay_counts_agreement_and_errors() {
        let corpus = Corpus {
            entries: vec![
                entry("delete", "", true),
                entry("keep", "    // a", true),
                entry("rewrite", "    // Counts retries.", true),
            ],
        };
        let results = ReplayResults {
            answers: vec![
                // agrees
                ReplayAnswer {
                    model: "m1".into(),
                    entry: 0,
                    action: Some("delete".into()),
                    comment: String::new(),
                    latency_ms: 100,
                    error: None,
                },
                // disagrees
                ReplayAnswer {
                    model: "m1".into(),
                    entry: 1,
                    action: Some("delete".into()),
                    comment: String::new(),
                    latency_ms: 300,
                    error: None,
                },
                // agrees, and proposed the text the human kept
                ReplayAnswer {
                    model: "m1".into(),
                    entry: 2,
                    action: Some("rewrite".into()),
                    comment: "Counts retries.".into(),
                    latency_ms: 200,
                    error: None,
                },
                // errored: must not count as a disagreement
                ReplayAnswer {
                    model: "m2".into(),
                    entry: 0,
                    action: None,
                    comment: String::new(),
                    latency_ms: 0,
                    error: Some("timed out".into()),
                },
            ],
        };

        let report = Report::from_replay(&corpus, &results);
        let m1 = &report.models["m1"];
        assert_eq!(m1.judged, 3);
        assert_eq!(m1.errors, 0);
        assert_eq!(m1.action_agreed, 2);
        assert!(
            (m1.agreement_pct() - 66.6).abs() < 1.0,
            "{}",
            m1.agreement_pct()
        );
        assert_eq!(
            m1.accepted, 1,
            "the matching rewrite should count as accepted"
        );
        assert_eq!(m1.mean_latency_ms(), 200);

        let m2 = &report.models["m2"];
        assert_eq!(m2.errors, 1);
        // One error out of one attempt leaves nothing to agree about, and must
        // not read as 0% agreement — that would punish a model for a crash the
        // same as for a wrong answer.
        assert_eq!(m2.agreement_pct(), 0.0);
        assert_eq!(m2.judged.saturating_sub(m2.errors), 0);
    }

    #[test]
    fn only_the_final_model_turn_is_scored() {
        let dir = crate::testkit::TempDir::new("final-turn");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let session = db.new_session("C:/work/widgets", "branch", "feature", "main");
        let judged = unit("src/lib.rs", 2);
        db.log_suggestion(
            session,
            &judged.file,
            2,
            2,
            "m1",
            Some("keep"),
            Some(""),
            Some("first answer"),
            10,
            None,
        );
        db.log_suggestion(
            session,
            &judged.file,
            2,
            2,
            "m1",
            Some("rewrite"),
            Some("better"),
            Some("follow-up answer"),
            20,
            None,
        );
        log_label(&db, session, &judged, "rewrite", "m1");

        let report = Report::from_db(&db);
        let score = &report.models["m1"];
        assert_eq!(score.judged, 1);
        assert_eq!(score.action_agreed, 1);
        assert_eq!(score.accepted, 1);
    }

    #[test]
    fn repeat_identity_includes_repository_and_unit_occurrence() {
        let dir = crate::testkit::TempDir::new("repeat-identity");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let repo_a = db.new_session("C:/repo/a", "branch", "feature", "main");
        let repo_a_again = db.new_session("C:/repo/a", "re-check", "past", "n/a");
        let repo_b = db.new_session("C:/repo/b", "branch", "feature", "main");
        let at_two = unit("src/lib.rs", 2);
        let at_nine = unit("src/lib.rs", 9);

        log_label(&db, repo_a, &at_two, "keep", "original");
        log_label(&db, repo_a_again, &at_two, "delete", "human-authored");
        log_label(&db, repo_b, &at_two, "keep", "original");
        log_label(&db, repo_a, &at_nine, "keep", "original");

        let report = Report::from_db(&db);
        assert_eq!(report.repeat_total, 1);
        assert_eq!(report.repeat_agreed, 0);
    }

    #[test]
    fn the_report_says_what_it_does_not_know() {
        let corpus = Corpus {
            entries: vec![entry("keep", "    // a", false)],
        };
        let results = ReplayResults { answers: vec![] };
        let text = Report::from_replay(&corpus, &results).render();
        assert!(text.contains("self-agreement: unknown"), "{text}");
        assert!(
            text.contains("names\n visible") || text.contains("names"),
            "{text}"
        );
        assert!(text.contains("not with any ground truth"), "{text}");
    }
}
