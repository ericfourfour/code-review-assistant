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
    /// The text the human chose (empty for a deletion).
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
    let model_configs = settings.enabled_models();
    let total = corpus.entries.len() * model_configs.len();
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
        for (_, model_config) in &model_configs {
            // A replay is one-shot per entry, so no session id is threaded
            // through; `{session}` would be meaningless without a follow-up.
            let command = model_config
                .command
                .replace("{session}", &uuid::Uuid::new_v4().to_string());
            let (result, _raw) =
                models::run_for_eval(model_config, &command, &prompt, &repo, &cli_home, timeout);
            let answer = match result {
                Ok(s) => ReplayAnswer {
                    model: model_config.name.clone(),
                    entry: idx,
                    action: Some(s.action.as_str().to_string()),
                    comment: s.comment,
                    latency_ms: s.latency_ms,
                    error: None,
                },
                Err(e) => ReplayAnswer {
                    model: model_config.name.clone(),
                    entry: idx,
                    action: None,
                    comment: String::new(),
                    latency_ms: 0,
                    error: Some(e),
                },
            };
            done += 1;
            progress(done, total, &model_config.name);
            answers.push(answer);
        }
    }
    ReplayResults { answers }
}

/// What the evaluation page is looking at: all of the history, or one
/// repository, and whether unblinded judgements count.
#[derive(Clone, PartialEq)]
pub struct Filter {
    /// `None` means every repository.
    pub repo: Option<String>,
    /// Drop decisions made with the model names visible. On by default: a
    /// choice made while looking at the names measures the reviewer's prior
    /// as much as the suggestion, and mixing the two into one rate hides
    /// which is which.
    pub blinded_only: bool,
}

impl Default for Filter {
    fn default() -> Self {
        Filter {
            repo: None,
            blinded_only: true,
        }
    }
}

/// One model's standing: how often the reviewer took its text when it was on
/// the table, and what that cost.
#[derive(Clone, Default)]
pub struct Standing {
    pub model: String,
    /// Contests it answered — the denominator for a win rate. An answer that
    /// errored is not on the table, so it is not counted here.
    pub offered: usize,
    /// Contests where the reviewer's text came from this model.
    pub wins: usize,
    /// Of those, ones the reviewer reworded before saving: still a win, but
    /// the model did not finish the job.
    pub wins_edited: usize,
    /// Answered with the same keep/rewrite/delete verdict as the reviewer.
    /// Cheaper to earn than a win — agreeing that a comment should be
    /// rewritten says nothing about whether the rewrite was any good.
    pub agreed: usize,
    /// Contests where it answered but errored out.
    pub errors: usize,
    pub latency_ms_total: i64,
    /// How its answers were distributed across the verdicts, so a model that
    /// says "rewrite" to everything is visible as such.
    pub verdicts: BTreeMap<String, usize>,
    /// Spend over every call, including calls on units that were skipped and
    /// never became a contest.
    pub calls: usize,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
    pub priced_calls: usize,
    pub estimated_calls: usize,
}

impl Standing {
    pub fn win_pct(&self) -> f64 {
        pct(self.wins, self.offered)
    }

    pub fn agreement_pct(&self) -> f64 {
        pct(self.agreed, self.offered)
    }

    pub fn error_pct(&self) -> f64 {
        pct(self.errors, self.offered + self.errors)
    }

    pub fn mean_latency_ms(&self) -> i64 {
        if self.offered == 0 {
            0
        } else {
            self.latency_ms_total / self.offered as i64
        }
    }

    pub fn tokens(&self) -> i64 {
        self.input_tokens + self.output_tokens
    }

    /// Mean cost of one call, over the calls that carry a cost at all.
    pub fn cost_per_call(&self) -> Option<f64> {
        (self.priced_calls > 0).then(|| self.cost_usd / self.priced_calls as f64)
    }

    /// What a suggestion the reviewer actually took cost.
    ///
    /// Note the two halves are scoped differently and cannot be otherwise:
    /// spend covers every call, while wins only cover the decisions the
    /// filter is scoring, because a call on a unit that was never decided has
    /// no blinding to filter by. The page says this beside the column.
    ///
    /// The number that matters when two models are close on quality and far
    /// apart on price —
    /// and it is `None` rather than infinite when nothing has been won yet,
    /// because "no wins" is a fact about the win column, not about cost.
    pub fn cost_per_win(&self) -> Option<f64> {
        (self.wins > 0 && self.priced_calls > 0).then(|| self.cost_usd / self.wins as f64)
    }
}

/// One pair of models, over the contests where both answered.
#[derive(Clone)]
pub struct HeadToHead {
    pub a: String,
    pub b: String,
    /// Contests both answered.
    pub together: usize,
    pub a_wins: usize,
    pub b_wins: usize,
}

impl HeadToHead {
    /// Contests where one of the two won. The rest went to the original text,
    /// to the reviewer's own words, or to a third model.
    pub fn decided(&self) -> usize {
        self.a_wins + self.b_wins
    }
}

fn pct(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        100.0 * n as f64 / d as f64
    }
}

/// Whether `source` — the provenance string on a decision — names `model`, and
/// if so whether the reviewer reworded the text before saving. A picked
/// suggestion that was then edited is stored as `<model>+human-edited`, and
/// that is still the model's text winning.
fn source_names(source: &str, model: &str) -> Option<bool> {
    if source == model {
        return Some(false);
    }
    source
        .strip_prefix(model)
        .and_then(|rest| (rest == "+human-edited").then_some(true))
}

/// The whole comparison, aggregated from the review history.
///
/// Every rate here is agreement with one reviewer, and the honest reading of
/// a small lead is "no difference yet" — which is why [`Leaderboard::caveats`]
/// exists and why the page prints it beside the table rather than under it.
pub struct Leaderboard {
    pub standings: Vec<Standing>,
    pub head_to_head: Vec<HeadToHead>,
    /// Decisions at least one model answered.
    pub contests: usize,
    /// Of those, ones where more than one model answered — the only ones where
    /// a win means something was actually beaten.
    pub contested: usize,
    /// Contests won by some model, by the reviewer's own words, and by leaving
    /// the original alone. These three account for every contest.
    pub model_won: usize,
    pub human_won: usize,
    pub original_kept: usize,
    /// Contests decided with the names hidden.
    pub blinded: usize,
    /// How the reviewer's own verdicts were distributed, as the baseline a
    /// model's verdict mix is read against.
    pub human_verdicts: BTreeMap<String, usize>,
    /// Comments judged twice, and how often the second verdict matched the
    /// first — the noise floor no model can be scored above.
    pub repeat_total: usize,
    pub repeat_agreed: usize,
    /// Answers dropped for being unblinded, so the page can say what it left
    /// out rather than quietly shrinking.
    pub excluded_unblinded: usize,
}

impl Leaderboard {
    pub fn from_db(db: &Db, filter: &Filter) -> Leaderboard {
        let rows = db.contest_rows();

        // Collapse to the last answer each model gave on each decision.
        // Re-running the models on a unit (the review screen's `R`) writes a
        // fresh suggestion row without retiring the old one, and counting both
        // would let one unit vote twice — with the superseded earlier answer
        // dragging down whichever model was re-run.
        let mut latest: BTreeMap<(i64, String), usize> = BTreeMap::new();
        let mut excluded_unblinded = 0;
        for (i, row) in rows.iter().enumerate() {
            if filter.repo.as_ref().is_some_and(|r| *r != row.repo) {
                continue;
            }
            if filter.blinded_only && !row.blinded {
                excluded_unblinded += 1;
                continue;
            }
            latest.insert((row.decision_id, row.model.clone()), i);
        }

        let mut by_model: BTreeMap<String, Standing> = BTreeMap::new();
        // decision -> (models that answered, the model whose text was taken)
        let mut contests: BTreeMap<i64, (Vec<String>, Option<String>)> = BTreeMap::new();
        let mut human_verdicts: BTreeMap<String, usize> = BTreeMap::new();
        // Per decision, not per answer: three models looking at one unit is
        // still one verdict by the reviewer.
        let mut decision_facts: BTreeMap<i64, (bool, String, String)> = BTreeMap::new();

        for ((decision_id, model), idx) in &latest {
            let row = &rows[*idx];
            let s = by_model.entry(model.clone()).or_default();
            s.model = model.clone();
            let entry = contests.entry(*decision_id).or_default();
            decision_facts.insert(
                *decision_id,
                (
                    row.blinded,
                    row.human_source.clone(),
                    row.human_action.clone(),
                ),
            );

            if row.error.is_some() {
                s.errors += 1;
                continue;
            }
            s.offered += 1;
            s.latency_ms_total += row.latency_ms;
            entry.0.push(model.clone());
            if let Some(action) = &row.model_action {
                *s.verdicts.entry(action.clone()).or_default() += 1;
                if *action == row.human_action {
                    s.agreed += 1;
                }
            }
            if let Some(edited) = source_names(&row.human_source, model) {
                s.wins += 1;
                s.wins_edited += usize::from(edited);
                entry.1 = Some(model.clone());
            }
        }

        let mut model_won = 0;
        let mut human_won = 0;
        let mut original_kept = 0;
        let mut blinded = 0;
        for (was_blinded, source, action) in decision_facts.values() {
            *human_verdicts.entry(action.clone()).or_default() += 1;
            blinded += usize::from(*was_blinded);
            match source.as_str() {
                "human-authored" => human_won += 1,
                "original" => original_kept += 1,
                _ => model_won += 1,
            }
        }

        // Spend is counted over every call, so it comes from the suggestions
        // table directly rather than from the contests above: a unit that was
        // skipped cost exactly as much to ask about as one that was decided.
        for row in db.spend_rows(filter.repo.as_deref()) {
            let s = by_model.entry(row.model.clone()).or_default();
            s.model = row.model;
            s.calls = row.calls;
            s.input_tokens = row.input_tokens;
            s.output_tokens = row.output_tokens;
            s.cache_read_tokens = row.cache_read_tokens;
            s.cost_usd = row.cost_usd;
            s.priced_calls = row.priced_calls;
            s.estimated_calls = row.estimated_calls;
        }

        let head_to_head = pairs(&contests);

        let mut standings: Vec<Standing> = by_model.into_values().collect();
        // Most-won first, then the one with more contests behind the rate,
        // then the cheaper, then by name so the order never wobbles.
        standings.sort_by(|a, b| {
            b.win_pct()
                .total_cmp(&a.win_pct())
                .then(b.offered.cmp(&a.offered))
                .then(a.cost_usd.total_cmp(&b.cost_usd))
                .then(a.model.cmp(&b.model))
        });

        let (repeat_total, repeat_agreed) = repeat_consistency(db);

        Leaderboard {
            contests: contests.len(),
            contested: contests
                .values()
                .filter(|(models, _)| models.len() > 1)
                .count(),
            model_won,
            human_won,
            original_kept,
            blinded,
            standings,
            head_to_head,
            human_verdicts,
            repeat_total,
            repeat_agreed,
            excluded_unblinded,
        }
    }

    pub fn self_agreement_pct(&self) -> Option<f64> {
        (self.repeat_total > 0).then(|| pct(self.repeat_agreed, self.repeat_total))
    }

    pub fn total_cost(&self) -> f64 {
        self.standings.iter().map(|s| s.cost_usd).sum()
    }

    pub fn total_tokens(&self) -> i64 {
        self.standings.iter().map(|s| s.tokens()).sum()
    }

    /// Calls whose cost nobody knows: the CLI did not price them and the model configuration
    /// has no rates. The difference between "cheap" and "unmeasured", which
    /// the page has to say out loud or the cost column lies by omission.
    pub fn unpriced_calls(&self) -> usize {
        self.standings
            .iter()
            .map(|s| s.calls.saturating_sub(s.priced_calls))
            .sum()
    }

    /// Everything that should temper a reading of the table, worst first.
    /// Empty means the numbers are as trustworthy as this design gets — which
    /// is still only "agrees with you", never "is right".
    pub fn caveats(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.contests == 0 {
            out.push(
                "No decisions to score yet. A contest is recorded when you save a verdict on a \
                 unit the models answered."
                    .into(),
            );
            return out;
        }
        if self.contested < MIN_CONTESTS {
            out.push(format!(
                "Only {} decision(s) had more than one model answering. That is too few to \
                 separate models: read the shape of the data, not the ranking.",
                self.contested
            ));
        }
        match self.self_agreement_pct() {
            Some(p) if p < 95.0 => out.push(format!(
                "You agreed with your own earlier verdict {p:.0}% of the time over {} repeated \
                 comment(s). No model can be scored meaningfully above that, so gaps smaller \
                 than {:.0} points are noise.",
                self.repeat_total,
                100.0 - p
            )),
            Some(_) => {}
            None => out.push(
                "No comment has been judged twice, so there is no noise floor to compare \
                 against. A few points between two models is indistinguishable from you \
                 having been in a different mood."
                    .into(),
            ),
        }
        if self.blinded < self.contests {
            out.push(format!(
                "{} of {} scored decisions were made with the model names visible, so those \
                 labels partly measure which model you already trust.",
                self.contests - self.blinded,
                self.contests
            ));
        }
        let unpriced = self.unpriced_calls();
        if unpriced > 0 {
            out.push(format!(
            "{unpriced} call(s) carry no cost: their CLI does not report one and the model configuration \
                 has no rates. Those models are unmeasured on cost, not cheap — set $/Mtok in \
                 settings to price them."
            ));
        }
        out
    }
}

/// Below this many contested decisions the ranking is not worth reading, and
/// the page says so rather than letting a two-decision lead look like a result.
const MIN_CONTESTS: usize = 8;

/// Every pair of models that met, and who the reviewer picked when they did.
fn pairs(contests: &BTreeMap<i64, (Vec<String>, Option<String>)>) -> Vec<HeadToHead> {
    let mut map: BTreeMap<(String, String), HeadToHead> = BTreeMap::new();
    for (models, winner) in contests.values() {
        let mut sorted = models.clone();
        sorted.sort();
        sorted.dedup();
        for (i, a) in sorted.iter().enumerate() {
            for b in &sorted[i + 1..] {
                let h = map
                    .entry((a.clone(), b.clone()))
                    .or_insert_with(|| HeadToHead {
                        a: a.clone(),
                        b: b.clone(),
                        together: 0,
                        a_wins: 0,
                        b_wins: 0,
                    });
                h.together += 1;
                match winner.as_deref() {
                    Some(w) if w == a => h.a_wins += 1,
                    Some(w) if w == b => h.b_wins += 1,
                    _ => {}
                }
            }
        }
    }
    let mut out: Vec<HeadToHead> = map.into_values().collect();
    out.sort_by(|x, y| {
        y.together
            .cmp(&x.together)
            .then(x.a.cmp(&y.a))
            .then(x.b.cmp(&y.b))
    });
    out
}

/// How often a comment judged twice got the same verdict both times.
fn repeat_consistency(db: &Db) -> (usize, usize) {
    let mut total = 0;
    let mut agreed = 0;
    for (_, actions) in db.repeated_decisions() {
        for pair in actions.windows(2) {
            total += 1;
            if pair[0] == pair[1] {
                agreed += 1;
            }
        }
    }
    (total, agreed)
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

    fn log_label(db: &Db, session_id: i64, unit: &ReviewUnit, action: &str, source: &str) {
        let original = unit.raw_lines().join("\n");
        let unit_json = serde_json::to_string(unit).unwrap();
        db.log_decision(&crate::db::DecisionRecord {
            session_id,
            file: unit.file(),
            line_start: unit.start_line(),
            line_end: unit.end_line(),
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

    /// A review as the database sees it: every model's answer on one unit,
    /// then the verdict the human saved. `winner` is the decision's `source`.
    struct Contest<'a> {
        line: u32,
        /// (model, action, tokens, cost) per answer; a `None` action is an
        /// answer that errored.
        answers: &'a [(&'a str, Option<&'a str>, i64, Option<f64>)],
        human_action: &'a str,
        winner: &'a str,
        blinded: bool,
    }

    fn record(db: &Db, session: i64, c: &Contest) {
        for (model, action, tokens, cost) in c.answers {
            db.log_suggestion(&crate::db::SuggestionRecord {
                session_id: session,
                file: "src/lib.rs",
                line_start: c.line,
                line_end: c.line,
                model,
                action: *action,
                comment: Some("text"),
                justification: None,
                latency_ms: 100,
                error: action.is_none().then_some("timed out"),
                evidence: None,
                usage: (*tokens > 0).then_some(models::Usage {
                    input_tokens: *tokens,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cost_usd: *cost,
                }),
                cost: cost.map(|usd| (usd, false)),
                follow_up_id: None,
                round: 1,
                stopped: false,
            });
        }
        let unit = unit("src/lib.rs", c.line);
        db.log_decision(&crate::db::DecisionRecord {
            session_id: session,
            file: "src/lib.rs",
            line_start: c.line,
            line_end: c.line,
            original: "    // a",
            action: c.human_action,
            final_text: "    // b",
            source: c.winner,
            human_edited: c.winner.ends_with("+human-edited"),
            committed: false,
            commit_sha: None,
            justification: None,
            unit_json: Some(&serde_json::to_string(&unit).unwrap()),
            blinded: c.blinded,
        });
    }

    fn answers<'a>(
        list: &'a [(&'a str, Option<&'a str>)],
    ) -> Vec<(&'a str, Option<&'a str>, i64, Option<f64>)> {
        list.iter().map(|(m, a)| (*m, *a, 0, None)).collect()
    }

    fn standing<'a>(board: &'a Leaderboard, model: &str) -> &'a Standing {
        board
            .standings
            .iter()
            .find(|s| s.model == model)
            .expect("model should have a standing")
    }

    /// The core of the page: picking one model's text over another's is the
    /// label, and a win counts whether or not the reviewer then reworded it.
    #[test]
    fn taking_a_models_text_is_a_win_even_after_editing_it() {
        let dir = crate::testkit::TempDir::new("board_wins");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let session = db.new_session("C:/work/widgets", "branch", "feature", "main");

        record(
            &db,
            session,
            &Contest {
                line: 2,
                answers: &answers(&[("a", Some("rewrite")), ("b", Some("rewrite"))]),
                human_action: "rewrite",
                winner: "a",
                blinded: true,
            },
        );
        record(
            &db,
            session,
            &Contest {
                line: 6,
                answers: &answers(&[("a", Some("rewrite")), ("b", Some("keep"))]),
                human_action: "rewrite",
                winner: "a+human-edited",
                blinded: true,
            },
        );
        record(
            &db,
            session,
            &Contest {
                line: 9,
                answers: &answers(&[("a", Some("delete")), ("b", Some("rewrite"))]),
                human_action: "rewrite",
                winner: "b",
                blinded: true,
            },
        );

        let board = Leaderboard::from_db(&db, &Filter::default());
        let a = standing(&board, "a");
        assert_eq!(a.offered, 3);
        assert_eq!(
            a.wins, 2,
            "a reworded suggestion is still that model's text winning"
        );
        assert_eq!(a.wins_edited, 1);
        assert_eq!(
            a.agreed, 2,
            "delete against the human's rewrite is a disagreement"
        );
        assert!((a.win_pct() - 66.6).abs() < 1.0, "{}", a.win_pct());

        let b = standing(&board, "b");
        assert_eq!(b.wins, 1);
        // Sorted by win rate, so the model whose text was taken more often
        // heads the table.
        assert_eq!(board.standings[0].model, "a");
        assert_eq!(board.contests, 3);
        assert_eq!(board.contested, 3);
        assert_eq!(board.model_won, 3);

        // Head to head is the same three contests read pairwise.
        let h = &board.head_to_head[0];
        assert_eq!((h.a.as_str(), h.b.as_str()), ("a", "b"));
        assert_eq!((h.together, h.a_wins, h.b_wins), (3, 2, 1));
    }

    /// Re-running the models on a unit writes fresh suggestion rows beside the
    /// old ones. Counting both would let one decision vote twice, and the
    /// superseded answer — the reason for the re-run — would drag its model
    /// down for a suggestion the reviewer never saw a verdict on.
    #[test]
    fn re_running_a_unit_counts_once_using_the_latest_answer() {
        let dir = crate::testkit::TempDir::new("board_rerun");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let session = db.new_session("C:/work/widgets", "branch", "feature", "main");

        // First pass: a says keep. Re-run: a says rewrite, which is what the
        // reviewer answered and took.
        record(
            &db,
            session,
            &Contest {
                line: 2,
                answers: &answers(&[("a", Some("keep"))]),
                human_action: "rewrite",
                winner: "a",
                blinded: true,
            },
        );
        db.log_suggestion(&crate::db::SuggestionRecord {
            session_id: session,
            file: "src/lib.rs",
            line_start: 2,
            line_end: 2,
            model: "a",
            action: Some("rewrite"),
            comment: Some("text"),
            justification: None,
            latency_ms: 100,
            error: None,
            evidence: None,
            usage: None,
            cost: None,
            follow_up_id: None,
            round: 1,
            stopped: false,
        });

        let board = Leaderboard::from_db(&db, &Filter::default());
        let a = standing(&board, "a");
        assert_eq!(a.offered, 1, "one decision, one vote");
        assert_eq!(a.wins, 1);
        assert_eq!(
            a.agreed, 1,
            "the answer that stood at decision time is the one scored"
        );
        assert_eq!(board.contests, 1);
    }

    /// An unblinded choice partly measures which model the reviewer already
    /// trusts. The default view drops those, and says how many it dropped
    /// rather than quietly shrinking.
    #[test]
    fn unblinded_decisions_are_excluded_by_default_and_counted() {
        let dir = crate::testkit::TempDir::new("board_blind");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let session = db.new_session("C:/work/widgets", "branch", "feature", "main");
        record(
            &db,
            session,
            &Contest {
                line: 2,
                answers: &answers(&[("a", Some("rewrite"))]),
                human_action: "rewrite",
                winner: "a",
                blinded: true,
            },
        );
        record(
            &db,
            session,
            &Contest {
                line: 6,
                answers: &answers(&[("a", Some("rewrite"))]),
                human_action: "rewrite",
                winner: "a",
                blinded: false,
            },
        );

        let blinded = Leaderboard::from_db(&db, &Filter::default());
        assert_eq!(blinded.contests, 1);
        assert_eq!(blinded.excluded_unblinded, 1);
        assert_eq!(standing(&blinded, "a").offered, 1);
        assert!(blinded
            .caveats()
            .iter()
            .all(|c| !c.contains("names visible")));

        let all = Leaderboard::from_db(
            &db,
            &Filter {
                repo: None,
                blinded_only: false,
            },
        );
        assert_eq!(all.contests, 2);
        assert_eq!(all.excluded_unblinded, 0);
        assert!(
            all.caveats().iter().any(|c| c.contains("names visible")),
            "including them has to come with the warning: {:?}",
            all.caveats()
        );
    }

    /// A model that errors is not on the table, so the contest it missed must
    /// not count against its win rate — that would score a crash the same as a
    /// suggestion the reviewer rejected.
    #[test]
    fn an_errored_answer_is_not_a_loss() {
        let dir = crate::testkit::TempDir::new("board_err");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let session = db.new_session("C:/work/widgets", "branch", "feature", "main");
        record(
            &db,
            session,
            &Contest {
                line: 2,
                answers: &answers(&[("a", Some("rewrite")), ("b", None)]),
                human_action: "rewrite",
                winner: "a",
                blinded: true,
            },
        );

        let board = Leaderboard::from_db(&db, &Filter::default());
        let b = standing(&board, "b");
        assert_eq!(b.offered, 0);
        assert_eq!(b.errors, 1);
        assert_eq!(b.win_pct(), 0.0);
        assert_eq!(b.error_pct(), 100.0);
        // One model answering is not a contest anyone won against anyone.
        assert_eq!(board.contested, 0);
        assert!(board.head_to_head.is_empty());
    }

    /// Spend covers every call, including the ones on units that were never
    /// decided: skipping a unit does not refund what it cost to ask about.
    #[test]
    fn spend_counts_calls_that_never_reached_a_decision() {
        let dir = crate::testkit::TempDir::new("board_spend");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let session = db.new_session("C:/work/widgets", "branch", "feature", "main");
        record(
            &db,
            session,
            &Contest {
                line: 2,
                answers: &[("a", Some("rewrite"), 500, Some(0.02))],
                human_action: "rewrite",
                winner: "a",
                blinded: true,
            },
        );
        // A unit the reviewer skipped: the call happened, no decision followed.
        db.log_suggestion(&crate::db::SuggestionRecord {
            session_id: session,
            file: "src/lib.rs",
            line_start: 40,
            line_end: 40,
            model: "a",
            action: Some("keep"),
            comment: Some("text"),
            justification: None,
            latency_ms: 100,
            error: None,
            evidence: None,
            usage: Some(models::Usage {
                input_tokens: 300,
                output_tokens: 0,
                cache_read_tokens: 0,
                cost_usd: Some(0.01),
            }),
            cost: Some((0.01, false)),
            follow_up_id: None,
            round: 1,
            stopped: false,
        });

        let board = Leaderboard::from_db(&db, &Filter::default());
        let a = standing(&board, "a");
        assert_eq!(a.offered, 1, "one contest");
        assert_eq!(a.calls, 2, "two calls, one of which was never decided");
        assert_eq!(a.input_tokens, 800);
        assert!((a.cost_usd - 0.03).abs() < 1e-9, "{}", a.cost_usd);
        assert_eq!(a.priced_calls, 2);
        assert_eq!(board.unpriced_calls(), 0);
        // The money question: what a suggestion you actually took cost.
        let per_win = a.cost_per_win().expect("one win, priced");
        assert!((per_win - 0.03).abs() < 1e-9, "{per_win}");
    }

    /// A model whose CLI reports nothing has to read as unmeasured. Showing it
    /// at zero would rank the least observable model as the cheapest.
    #[test]
    fn a_model_with_no_cost_reported_is_unmeasured_not_free() {
        let dir = crate::testkit::TempDir::new("board_unpriced");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let session = db.new_session("C:/work/widgets", "branch", "feature", "main");
        record(
            &db,
            session,
            &Contest {
                line: 2,
                answers: &answers(&[("quiet", Some("rewrite"))]),
                human_action: "rewrite",
                winner: "quiet",
                blinded: true,
            },
        );

        let board = Leaderboard::from_db(&db, &Filter::default());
        let s = standing(&board, "quiet");
        assert_eq!(s.priced_calls, 0);
        assert_eq!(s.cost_per_call(), None);
        assert_eq!(
            s.cost_per_win(),
            None,
            "a win is not free just because nobody priced it"
        );
        assert_eq!(board.unpriced_calls(), 1);
        assert!(
            board
                .caveats()
                .iter()
                .any(|c| c.contains("unmeasured on cost")),
            "{:?}",
            board.caveats()
        );
    }

    /// Two checkouts are two review jobs; a model that suits one may not suit
    /// the other, and the scope filter is what lets that be seen.
    #[test]
    fn the_repository_filter_scopes_both_the_contests_and_the_spend() {
        let dir = crate::testkit::TempDir::new("board_repo");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let widgets = db.new_session("C:/work/widgets", "branch", "feature", "main");
        let gadgets = db.new_session("C:/work/gadgets", "branch", "feature", "main");
        record(
            &db,
            widgets,
            &Contest {
                line: 2,
                answers: &[("a", Some("rewrite"), 100, Some(0.01))],
                human_action: "rewrite",
                winner: "a",
                blinded: true,
            },
        );
        record(
            &db,
            gadgets,
            &Contest {
                line: 2,
                answers: &[("a", Some("rewrite"), 100, Some(0.05))],
                human_action: "keep",
                winner: "original",
                blinded: true,
            },
        );

        let scoped = Leaderboard::from_db(
            &db,
            &Filter {
                repo: Some("C:/work/widgets".into()),
                blinded_only: true,
            },
        );
        assert_eq!(scoped.contests, 1);
        assert_eq!(standing(&scoped, "a").wins, 1);
        assert!((standing(&scoped, "a").cost_usd - 0.01).abs() < 1e-9);

        let everything = Leaderboard::from_db(&db, &Filter::default());
        assert_eq!(everything.contests, 2);
        assert_eq!(everything.model_won, 1);
        assert_eq!(
            everything.original_kept, 1,
            "keeping the original is nobody's win"
        );
        assert!((everything.total_cost() - 0.06).abs() < 1e-9);
        assert_eq!(db.repos_with_history().len(), 2);
    }

    /// The reviewer's own verdict mix is the baseline a model's mix is read
    /// against, so it has to be counted per decision — not once per model that
    /// happened to answer it.
    #[test]
    fn the_reviewers_verdict_mix_counts_decisions_not_answers() {
        let dir = crate::testkit::TempDir::new("board_mix");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let session = db.new_session("C:/work/widgets", "branch", "feature", "main");
        record(
            &db,
            session,
            &Contest {
                line: 2,
                answers: &answers(&[
                    ("a", Some("keep")),
                    ("b", Some("rewrite")),
                    ("c", Some("keep")),
                ]),
                human_action: "keep",
                winner: "original",
                blinded: true,
            },
        );
        record(
            &db,
            session,
            &Contest {
                line: 6,
                answers: &answers(&[("a", Some("delete"))]),
                human_action: "delete",
                winner: "a",
                blinded: true,
            },
        );

        let board = Leaderboard::from_db(&db, &Filter::default());
        assert_eq!(
            board.human_verdicts["keep"], 1,
            "three models looked at it; it is one verdict"
        );
        assert_eq!(board.human_verdicts["delete"], 1);
        assert_eq!(standing(&board, "a").verdicts["keep"], 1);
        assert_eq!(standing(&board, "a").verdicts["delete"], 1);
    }

    /// The page is not allowed to present a two-decision lead as a finding.
    #[test]
    fn a_thin_record_says_so_before_it_says_anything_else() {
        let dir = crate::testkit::TempDir::new("board_thin");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let empty = Leaderboard::from_db(&db, &Filter::default());
        assert_eq!(
            empty.caveats().len(),
            1,
            "nothing to score yet is the only thing to say"
        );
        assert!(empty.caveats()[0].contains("No decisions to score yet"));

        let session = db.new_session("C:/work/widgets", "branch", "feature", "main");
        record(
            &db,
            session,
            &Contest {
                line: 2,
                answers: &answers(&[("a", Some("rewrite")), ("b", Some("keep"))]),
                human_action: "rewrite",
                winner: "a",
                blinded: true,
            },
        );
        let board = Leaderboard::from_db(&db, &Filter::default());
        let caveats = board.caveats();
        assert!(
            caveats[0].contains("too few to separate models"),
            "{caveats:?}"
        );
        assert!(
            caveats.iter().any(|c| c.contains("no noise floor")),
            "with nothing judged twice there is no floor to compare against: {caveats:?}"
        );
        assert_eq!(board.self_agreement_pct(), None);
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
        db.log_suggestion(&crate::db::SuggestionRecord {
            session_id: session,
            file: judged.file(),
            line_start: 2,
            line_end: 2,
            model: "m1",
            action: Some("keep"),
            comment: Some(""),
            justification: Some("first answer"),
            latency_ms: 10,
            error: None,
            evidence: None,
            usage: None,
            cost: None,
            follow_up_id: None,
            round: 1,
        });
        db.log_suggestion(&crate::db::SuggestionRecord {
            session_id: session,
            file: judged.file(),
            line_start: 2,
            line_end: 2,
            model: "m1",
            action: Some("rewrite"),
            comment: Some("better"),
            justification: Some("follow-up answer"),
            latency_ms: 20,
            error: None,
            evidence: None,
            usage: None,
            cost: None,
            follow_up_id: None,
            round: 2,
        });
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
