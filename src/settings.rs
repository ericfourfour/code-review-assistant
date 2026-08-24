//! App settings, persisted as JSON in the sqlite `settings` table.

use serde::{Deserialize, Serialize};

use crate::db::Db;

/// One reviewer model backed by an installed, authenticated CLI.
/// `command` is a whitespace-tokenized template; the `{prompt}` token is
/// replaced with the prompt text. If no `{prompt}` token is present the
/// prompt is piped to the process on stdin. No shell is involved.
#[derive(Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub name: String,
    pub command: String,
    /// `Name <email>` used in the Co-authored-by trailer when this model's
    /// suggestion is picked.
    pub coauthor: String,
    pub enabled: bool,
    /// Model variant to request, e.g. "opus" or "claude-sonnet-5". Empty means
    /// "whatever the CLI defaults to". Appended as `<model_flag> <model>`.
    #[serde(default)]
    pub model: String,
    /// Flag the CLI uses to select a model (`--model` for all three defaults).
    #[serde(default = "default_model_flag")]
    pub model_flag: String,
    /// Reasoning-effort setting. Empty means "leave the CLI alone". Appended
    /// as `<effort_flag> <effort>`, so it also covers CLIs that expose effort
    /// as a config override rather than a dedicated flag (codex's `-c`).
    #[serde(default)]
    pub effort: String,
    #[serde(default = "default_effort_flag")]
    pub effort_flag: String,
    /// Command used to continue an existing conversation. `{session}` is
    /// replaced with the session id. Empty means the CLI has no resume mode,
    /// so follow-ups are unavailable for the model.
    #[serde(default)]
    pub resume_command: String,
    /// JSON key in the CLI output that carries the session id. Empty means
    /// the id is ours to generate — `command` must then contain `{session}`
    /// (that is claude's `--session-id <uuid>`).
    #[serde(default)]
    pub session_key: String,
    /// USD per million tokens, used to price a call whose CLI reports tokens
    /// but not money. Zero means "unpriced": the evaluation page then shows
    /// this model's tokens with no cost rather than pretending it was free.
    /// A CLI that reports its own cost is always believed over these.
    #[serde(default)]
    pub price_in: f64,
    #[serde(default)]
    pub price_out: f64,
}

fn default_model_flag() -> String {
    "--model".into()
}

fn default_effort_flag() -> String {
    "--effort".into()
}

fn default_blind_review() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_check_timeout() -> u64 {
    120
}

fn default_repo_max_age_days() -> u32 {
    180
}

/// Known effort settings for a command template, offered as a dropdown.
/// Free-text like the model field: shortcuts, not a whitelist.
pub fn effort_presets(command: &str) -> &'static [&'static str] {
    match command.split_whitespace().next().unwrap_or("") {
        "claude" => &["low", "medium", "high", "xhigh", "max"],
        // codex has no --effort flag; it takes a config override instead.
        "codex" => &[
            "model_reasoning_effort=low",
            "model_reasoning_effort=medium",
            "model_reasoning_effort=high",
        ],
        // agy has --effort, but its model names already carry the level
        // (gemini-3.7-flash-low), so picking the model is usually enough.
        "agy" => &["low", "medium", "high"],
        _ => &[],
    }
}

/// Known model names for a command template, offered as a dropdown in
/// settings. The field stays free-text: these are shortcuts, not a whitelist.
pub fn model_presets(command: &str) -> &'static [&'static str] {
    match command.split_whitespace().next().unwrap_or("") {
        "claude" => &[
            "opus",
            "sonnet",
            "haiku",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-haiku-4-5",
            "claude-opus-4-8",
        ],
        // Codex has no listing subcommand; these are what `/models` reports,
        // each confirmed to be accepted by `codex exec --model`.
        "codex" => &[
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.4",
        ],
        // `agy models` lists these; keep the common ones handy.
        "agy" => &[
            "gemini-3.7-flash-low",
            "gemini-3.7-flash-medium",
            "gemini-3.7-flash-high",
            "gemini-3.1-pro-low",
            "gemini-3.1-pro-high",
            "claude-sonnet-4-6",
            "claude-opus-4-6-thinking",
        ],
        _ => &[],
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub models: Vec<ModelConfig>,
    /// Fallback base branch when origin/HEAD cannot be resolved.
    pub default_base: String,
    /// Command used for PR listing/checkout (normally `gh`).
    pub gh_path: String,
    /// Seconds before a model CLI call is abandoned.
    pub model_timeout_secs: u64,
    /// Context lines shown around a comment in the prompt and the UI.
    pub context_lines: usize,
    pub recent_repos: Vec<String>,
    /// Discovered repositories with no activity in this many days are hidden
    /// from the picker. 0 shows everything ever found.
    #[serde(default = "default_repo_max_age_days")]
    pub repo_max_age_days: u32,
    /// Repositories the picker must not offer again — local paths or GitHub
    /// `owner/name` slugs. Grown from the picker's exclude action, edited here.
    #[serde(default)]
    pub excluded_repos: Vec<String>,
    /// Where a repository that only exists on GitHub is cloned when opened.
    /// Empty means the home folder, next to where the scan looks.
    #[serde(default)]
    pub clone_dir: String,
    /// Hide which model produced each candidate, and shuffle their order, until
    /// a choice is made. Reviewing is also labelling: seeing the name while
    /// choosing biases the label toward whichever model you already trust, and
    /// a fixed left-to-right order biases it toward the first model.
    #[serde(default = "default_blind_review")]
    pub blind_review: bool,
    /// Review the comment runs the branch touched (the original flow).
    #[serde(default = "default_true")]
    pub review_comments: bool,
    /// Review the code the branch changed — semantic units where the language
    /// allows, hunk units everywhere else.
    #[serde(default = "default_true")]
    pub review_code: bool,
    /// Validation command run in the repository after each applied edit
    /// (e.g. `cargo check`, `tsc --noEmit`). Whitespace-tokenized, no shell.
    /// Empty disables validation. An edit whose check fails is reverted.
    #[serde(default)]
    pub check_command: String,
    #[serde(default = "default_check_timeout")]
    pub check_timeout_secs: u64,
    /// Also run the check after comment-only edits. Off by default: a comment
    /// edit rarely breaks a build, and checks are slow.
    #[serde(default)]
    pub validate_comment_edits: bool,
    /// Working-tree reviews also take in untracked files, rendered as
    /// new-file diffs. Off by default: untracked often means scratch files,
    /// and `git diff HEAD` not showing them is what git users expect.
    #[serde(default)]
    pub include_untracked: bool,
    /// Review the plan riskiest-unit-first (local heuristic score) instead of
    /// diff order, so the changes most likely to need attention come first.
    #[serde(default = "default_true")]
    pub triage_order: bool,
    /// Leave units out of a new plan when this repository already has a
    /// verdict on them. A decision is meant to stick: without this, every
    /// session re-offers everything the branch touched, and the work of
    /// reviewing is paid again each time the app is opened.
    #[serde(default = "default_true")]
    pub skip_decided: bool,
    /// Prepend the reviewer's standing preferences — mined from past follow-up
    /// questions and verdicts — to every review prompt, so the models apply
    /// them from round one instead of waiting to be corrected per unit.
    #[serde(default = "default_true")]
    pub send_profile: bool,
    /// Query the models for the next unit while the current one is being
    /// decided. Deciding takes minutes and the models seconds, so by the time
    /// the review advances the verdicts are usually already in.
    #[serde(default = "default_true")]
    pub prefetch_next: bool,
    /// When a prefetched unit comes back with every model saying keep, push it
    /// to the end of its file so the units the models disagree about — the
    /// ones worth attention — are reviewed first.
    #[serde(default = "default_true")]
    pub defer_unanimous_keeps: bool,
    /// Stop launching model calls once this run of the app has spent this many
    /// USD. Zero means no ceiling.
    ///
    /// Measured against what the CLIs themselves reported, never against an
    /// estimate: a model whose CLI is silent about spend cannot be counted,
    /// and stopping the work on a guess would stop it for the wrong reason.
    /// The ceiling is per run rather than per day because that is the window
    /// this app can actually account for — it sees its own calls and nothing
    /// else the same CLIs may be doing elsewhere on the machine.
    #[serde(default)]
    pub usage_limit_usd: f64,
    /// Stop launching model calls once this run has spent this many tokens
    /// (input plus output). Zero means no ceiling. Useful for the CLIs that
    /// count tokens but never price them.
    #[serde(default)]
    pub usage_limit_tokens: i64,
}

/// A model that reads its way around the repository before answering takes far
/// longer than one that only reads the hunk, so the ceiling that was generous
/// for a single-shot verdict is not generous for a browsing one.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            models: vec![
                ModelConfig {
                    name: "claude".into(),
                    command: CLAUDE_CMD.into(),
                    coauthor: "Claude <noreply@anthropic.com>".into(),
                    enabled: true,
                    model: CLAUDE_MODEL.into(),
                    model_flag: "--model".into(),
                    effort: CLAUDE_EFFORT.into(),
                    effort_flag: "--effort".into(),
                    resume_command: CLAUDE_RESUME.into(),
                    session_key: String::new(),
                    // Claude's CLI prices its own calls, so these stay unset;
                    // they exist for a model whose CLI only counts tokens.
                    price_in: 0.0,
                    price_out: 0.0,
                },
                ModelConfig {
                    name: "codex".into(),
                    command: CODEX_CMD.into(),
                    coauthor: "Codex <codex@openai.com>".into(),
                    enabled: true,
                    model: CODEX_MODEL.into(),
                    model_flag: "--model".into(),
                    effort: CODEX_EFFORT.into(),
                    effort_flag: "-c".into(),
                    resume_command: CODEX_RESUME.into(),
                    session_key: "thread_id".into(),
                    price_in: 0.0,
                    price_out: 0.0,
                },
                // agy has no stdin mode, but it is a native .exe, so the
                // prompt goes as an argument (32k limit, not cmd.exe's 8k).
                ModelConfig {
                    name: "agy".into(),
                    command: AGY_CMD.into(),
                    coauthor: "Antigravity <antigravity@google.com>".into(),
                    enabled: true,
                    model: AGY_MODEL.into(),
                    model_flag: "--model".into(),
                    // agy encodes the effort level in the model name, so the
                    // separate flag would be redundant here.
                    effort: String::new(),
                    effort_flag: "--effort".into(),
                    resume_command: AGY_RESUME.into(),
                    session_key: "conversation_id".into(),
                    price_in: 0.0,
                    price_out: 0.0,
                },
            ],
            default_base: "main".into(),
            gh_path: "gh".into(),
            model_timeout_secs: DEFAULT_TIMEOUT_SECS,
            context_lines: 12,
            recent_repos: Vec::new(),
            repo_max_age_days: default_repo_max_age_days(),
            excluded_repos: Vec::new(),
            clone_dir: String::new(),
            blind_review: default_blind_review(),
            review_comments: true,
            review_code: true,
            check_command: String::new(),
            check_timeout_secs: default_check_timeout(),
            validate_comment_edits: false,
            include_untracked: false,
            triage_order: true,
            skip_decided: true,
            send_profile: true,
            prefetch_next: true,
            defer_unanimous_keeps: true,
            // No ceiling by default: a limit the reviewer did not ask for
            // would stop a review part-way with no warning.
            usage_limit_usd: 0.0,
            usage_limit_tokens: 0,
        }
    }
}

const KEY: &str = "settings";

// The shipped command for each CLI, and the command that continues its
// conversation. Each first call asks for machine-readable output so the
// session id can be recovered — except claude, which lets us name the id.
//
// Every template also grants the CLI read-only run of the repository it is
// started in, because a comment cannot be judged from its own hunk alone. The
// three CLIs spell that differently: claude takes a tool allowlist and codex a
// sandbox mode, both of which apply to wherever the process was started.
//
// agy scopes access to a workspace instead of a working directory, and reading
// inside that workspace is allowed without a prompt — so it is handed the
// repository with `--add-dir {repo}`, which is what makes its own read and
// search tools usable rather than blocked. Plan mode keeps it off the working
// tree, which matters here because a workspace is writable by default and the
// edits are ours to apply.
//
// Its permissions live in a settings file rather than in flags, so it is also
// given a home of its own with `--gemini_dir={cli_home}`; see [`crate::agycli`]
// for what that home allows and why it is not the user's. Its `--sandbox` flag
// is deliberately absent: measured on Windows, a command run under it wants an
// admin escalation that print mode cannot prompt for, and the run dies with
// "context canceled".
// claude asks for `stream-json` rather than `json`: same envelope keys — the
// verdict arrives escaped inside the final result event, the usage beside it —
// but printed as one event per line *as they happen*, which is what feeds the
// live "what is it doing" view while a call is running. Print mode requires
// `--verbose` before it will stream.
const CLAUDE_CMD: &str = "claude -p --verbose --output-format stream-json \
                          --session-id {session} \
                          --tools Read,Grep,Glob --allowed-tools Read,Grep,Glob";
const CLAUDE_RESUME: &str = "claude -p --verbose --output-format stream-json \
                             --resume {session} \
                             --tools Read,Grep,Glob --allowed-tools Read,Grep,Glob";
const CODEX_CMD: &str = "codex exec --skip-git-repo-check --json --sandbox read-only";
const CODEX_RESUME: &str =
    "codex exec --skip-git-repo-check --json --sandbox read-only resume {session} -";
const AGY_CMD: &str = "agy --gemini_dir={cli_home} -p {prompt} --output-format json \
                       --mode plan --add-dir {repo}";
const AGY_RESUME: &str = "agy --gemini_dir={cli_home} -p {prompt} --output-format json \
                          --mode plan --add-dir {repo} --conversation {session}";

// Start small and cheap: a first-pass comment reviewer runs on every hunk, and
// the cheapest tier of each family handles "does this comment restate the
// code" perfectly well. Raise per model in settings when a repo needs it.
const CLAUDE_MODEL: &str = "haiku";
const CLAUDE_EFFORT: &str = "low";
const CODEX_MODEL: &str = "gpt-5.6-luna";
const CODEX_EFFORT: &str = "model_reasoning_effort=low";
const AGY_MODEL: &str = "gemini-3.7-flash-low";

impl Settings {
    pub fn load(db: &Db) -> Settings {
        db
            .get_setting(KEY)
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, db: &Db) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            db.set_setting(KEY, &json);
        }
    }

    pub fn remember_repo(&mut self, path: &str) {
        self.recent_repos.retain(|r| r != path);
        self.recent_repos.insert(0, path.to_string());
        self.recent_repos.truncate(15);
    }

    pub fn enabled_models(&self) -> Vec<(usize, ModelConfig)> {
        self.models
            .iter()
            .enumerate()
            .filter(|(_, m)| m.enabled)
            .map(|(i, m)| (i, m.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_commands_do_not_disable_cli_permissions() {
        // Read access is granted per CLI by naming what may be read, not by
        // approving whatever the model decides to run. A flag like this in a
        // default would hand every reviewed repository to an unattended agent.
        for model_config in Settings::default().models {
            for command in [&model_config.command, &model_config.resume_command] {
                assert!(
                    !command.contains("dangerously") && !command.contains("bypass"),
                    "{} ships a permission bypass: {command}",
                    model_config.name
                );
            }
        }
    }

    #[test]
    fn every_default_model_has_model_presets() {
        // A preset list that does not match its command is a silent dead end
        // in the settings dropdown, so tie them together.
        for model_config in Settings::default().models {
            let program = model_config.command.split_whitespace().next().unwrap_or("");
            assert!(
                !model_presets(program).is_empty(),
                "{} ({program}) offers no model presets",
                model_config.name
            );
        }
    }

    #[test]
    fn every_default_model_can_resume() {
        for model_config in Settings::default().models {
            assert!(
                !model_config.resume_command.is_empty(),
                "{} has no resume command",
                model_config.name
            );
            assert!(
                model_config.resume_command.contains("{session}"),
                "{} resume command must carry the session id",
                model_config.name
            );
            // Either the CLI reports the id, or we hand it one.
            assert!(
                !model_config.session_key.is_empty()
                    || model_config.command.contains("{session}"),
                "{} can neither report nor accept a session id",
                model_config.name
            );
        }
    }
}
