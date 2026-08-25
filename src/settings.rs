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
    /// Writable command used by follow-up fix sessions. Kept separate from the
    /// read-only review command so granting edit tools cannot leak into review.
    #[serde(default)]
    pub fix_command: String,
    /// Optional `Name <email>` used in the Co-authored-by trailer when this
    /// model's suggestion is picked. Empty records model provenance without
    /// claiming a GitHub identity.
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
    /// Writable resume command for the fix conversation.
    #[serde(default)]
    pub fix_resume_command: String,
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
    /// The preamble body sent in place of the mined one. Empty — the default —
    /// keeps the mined text, which stays current as follow-ups accumulate; a
    /// written one is sent verbatim under the same header until it is cleared.
    /// Ignored entirely when `send_profile` is off.
    #[serde(default)]
    pub reviewer_preferences: String,
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
    /// Bumped when a one-shot migration must repair shipped defaults.
    #[serde(default)]
    pub schema_version: u32,
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

const SCHEMA_VERSION: u32 = 3;

/// A model that reads its way around the repository before answering takes far
/// longer than one that only reads the hunk, so the ceiling that was generous
/// for a single-shot verdict is not generous for a browsing one.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
const PRE_BROWSE_TIMEOUT_SECS: u64 = 120;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            models: vec![
                ModelConfig {
                    name: "claude".into(),
                    command: CLAUDE_CMD.into(),
                    fix_command: CLAUDE_FIX.into(),
                    coauthor: "Claude <noreply@anthropic.com>".into(),
                    enabled: true,
                    model: CLAUDE_MODEL.into(),
                    model_flag: "--model".into(),
                    effort: CLAUDE_EFFORT.into(),
                    effort_flag: "--effort".into(),
                    resume_command: CLAUDE_RESUME.into(),
                    fix_resume_command: CLAUDE_FIX_RESUME.into(),
                    session_key: String::new(),
                    // Claude's CLI prices its own calls, so these stay unset;
                    // they exist for a model whose CLI only counts tokens.
                    price_in: 0.0,
                    price_out: 0.0,
                },
                ModelConfig {
                    name: "codex".into(),
                    command: CODEX_CMD.into(),
                    fix_command: CODEX_FIX.into(),
                    coauthor: "Codex <codex@openai.com>".into(),
                    enabled: true,
                    model: CODEX_MODEL.into(),
                    model_flag: "--model".into(),
                    effort: CODEX_EFFORT.into(),
                    effort_flag: "-c".into(),
                    resume_command: CODEX_RESUME.into(),
                    fix_resume_command: CODEX_FIX_RESUME.into(),
                    session_key: "thread_id".into(),
                    price_in: 0.0,
                    price_out: 0.0,
                },
                // agy has no stdin mode, but it is a native .exe, so the
                // prompt goes as an argument (32k limit, not cmd.exe's 8k).
                ModelConfig {
                    name: "agy".into(),
                    command: AGY_CMD.into(),
                    fix_command: AGY_FIX.into(),
                    // Google does not publish a GitHub co-author identity for
                    // Antigravity. The old antigravity@google.com placeholder
                    // is associated with an unrelated GitHub user, so rely on
                    // the agy provenance trailer instead.
                    coauthor: String::new(),
                    enabled: true,
                    model: AGY_MODEL.into(),
                    model_flag: "--model".into(),
                    // agy encodes the effort level in the model name, so the
                    // separate flag would be redundant here.
                    effort: String::new(),
                    effort_flag: "--effort".into(),
                    resume_command: AGY_RESUME.into(),
                    fix_resume_command: AGY_FIX_RESUME.into(),
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
            reviewer_preferences: String::new(),
            prefetch_next: true,
            defer_unanimous_keeps: true,
            schema_version: SCHEMA_VERSION,
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
const CLAUDE_FIX: &str = "claude -p --verbose --output-format stream-json \
                          --session-id {session} \
                          --tools Read,Grep,Glob,Edit,Write,Bash \
                          --allowed-tools Read,Grep,Glob,Edit,Write,Bash";
const CLAUDE_FIX_RESUME: &str = "claude -p --verbose --output-format stream-json \
                                 --resume {session} \
                                 --tools Read,Grep,Glob,Edit,Write,Bash \
                                 --allowed-tools Read,Grep,Glob,Edit,Write,Bash";
const CODEX_CMD: &str = "codex exec --skip-git-repo-check --json --sandbox read-only";
const CODEX_RESUME: &str =
    "codex exec --skip-git-repo-check --json --sandbox read-only resume {session} -";
const CODEX_FIX: &str = "codex exec --skip-git-repo-check --json --sandbox workspace-write";
const CODEX_FIX_RESUME: &str =
    "codex exec --skip-git-repo-check --json --sandbox workspace-write resume {session} -";
const AGY_CMD: &str = "agy --gemini_dir={cli_home} -p {prompt} --output-format json \
                       --mode plan --add-dir {repo}";
const AGY_RESUME: &str = "agy --gemini_dir={cli_home} -p {prompt} --output-format json \
                          --mode plan --add-dir {repo} --conversation {session}";
const AGY_FIX: &str = "agy --gemini_dir={cli_home} -p {prompt} --output-format json \
                       --add-dir {repo}";
const AGY_FIX_RESUME: &str = "agy --gemini_dir={cli_home} -p {prompt} --output-format json \
                              --add-dir {repo} --conversation {session}";

// Start small and cheap: a first-pass comment reviewer runs on every hunk, and
// the cheapest tier of each family handles "does this comment restate the
// code" perfectly well. Raise per model in settings when a repo needs it.
const CLAUDE_MODEL: &str = "haiku";
const CLAUDE_EFFORT: &str = "low";
const CODEX_MODEL: &str = "gpt-5.6-luna";
const CODEX_EFFORT: &str = "model_reasoning_effort=low";
const AGY_MODEL: &str = "gemini-3.7-flash-low";

/// Starting model/effort per shipped command, applied once by [`Settings::migrate`]
/// to installs that predate these fields: (command, model, effort, effort flag).
const STARTING_TIER: &[(&str, &str, &str, &str)] = &[
    (CLAUDE_CMD, CLAUDE_MODEL, CLAUDE_EFFORT, "--effort"),
    (CODEX_CMD, CODEX_MODEL, CODEX_EFFORT, "-c"),
    (AGY_CMD, AGY_MODEL, "", "--effort"),
];

/// Session wiring keyed by the command it belongs to: (command, resume, key).
const SESSION_WIRING: &[(&str, &str, &str)] = &[
    (CLAUDE_CMD, CLAUDE_RESUME, ""),
    (CODEX_CMD, CODEX_RESUME, "thread_id"),
    (AGY_CMD, AGY_RESUME, "conversation_id"),
];

/// Command templates that shipped in earlier versions, mapped to the current
/// one. Applied on load to both the opening and the resume template, so an
/// existing install picks up fixes and repository access automatically.
const FIX_WIRING: &[(&str, &str, &str)] = &[
    (CLAUDE_CMD, CLAUDE_FIX, CLAUDE_FIX_RESUME),
    (CODEX_CMD, CODEX_FIX, CODEX_FIX_RESUME),
    (AGY_CMD, AGY_FIX, AGY_FIX_RESUME),
];

const COMMAND_FIXUPS: &[(&str, &str)] = &[
    ("codex exec {prompt}", CODEX_CMD),
    ("codex exec --skip-git-repo-check", CODEX_CMD),
    ("codex exec --skip-git-repo-check --json", CODEX_CMD),
    (
        "codex exec --skip-git-repo-check --json resume {session} -",
        CODEX_RESUME,
    ),
    ("claude -p {prompt}", CLAUDE_CMD),
    ("claude -p", CLAUDE_CMD),
    ("claude -p --session-id {session}", CLAUDE_CMD),
    ("claude -p --resume {session}", CLAUDE_RESUME),
    (
        "claude -p --session-id {session} --tools Read,Grep,Glob --allowed-tools Read,Grep,Glob",
        CLAUDE_CMD,
    ),
    (
        "claude -p --resume {session} --tools Read,Grep,Glob --allowed-tools Read,Grep,Glob",
        CLAUDE_RESUME,
    ),
    ("agy -p {prompt}", AGY_CMD),
    ("agy --print -", AGY_CMD),
    ("agy -p {prompt} --output-format json", AGY_CMD),
    (
        "agy -p {prompt} --output-format json --conversation {session}",
        AGY_RESUME,
    ),
    (
        "agy -p {prompt} --output-format json --mode plan --add-dir {repo}",
        AGY_CMD,
    ),
    (
        "agy -p {prompt} --output-format json --mode plan --add-dir {repo} \
         --conversation {session}",
        AGY_RESUME,
    ),
];

impl Settings {
    pub fn load(db: &Db) -> Settings {
        let mut settings: Settings = db
            .get_setting(KEY)
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        settings.remove_unsafe_coauthors();
        settings.migrate();
        settings
    }

    /// Repair only exact shipped templates; user-authored commands remain
    /// untouched. New writable fix templates are filled for known reviewers.
    fn migrate(&mut self) -> bool {
        let mut changed = false;
        let fresh_fields = self.schema_version < 1;
        let repo_access = self.schema_version < 2;
        for model in &mut self.models {
            for template in [&mut model.command, &mut model.resume_command] {
                if let Some((_, fixed)) = COMMAND_FIXUPS
                    .iter()
                    .find(|(old, _)| template.trim() == *old)
                {
                    *template = (*fixed).to_string();
                    changed = true;
                }
            }
            if model.model_flag.trim().is_empty() {
                model.model_flag = default_model_flag();
                changed = true;
            }
            if model.effort_flag.trim().is_empty() {
                model.effort_flag = default_effort_flag();
                changed = true;
            }
            if fresh_fields {
                if let Some((_, tier, effort, effort_flag)) = STARTING_TIER
                    .iter()
                    .find(|(command, ..)| model.command.trim() == *command)
                {
                    if model.model.trim().is_empty() {
                        model.model = (*tier).to_string();
                    }
                    if model.effort.trim().is_empty() {
                        model.effort = (*effort).to_string();
                        model.effort_flag = (*effort_flag).to_string();
                    }
                    changed = true;
                }
            }
            if model.resume_command.trim().is_empty() {
                if let Some((_, resume, key)) = SESSION_WIRING
                    .iter()
                    .find(|(command, _, _)| model.command.trim() == *command)
                {
                    model.resume_command = (*resume).to_string();
                    model.session_key = (*key).to_string();
                    changed = true;
                }
            }
            if let Some((_, fix, fix_resume)) = FIX_WIRING
                .iter()
                .find(|(command, _, _)| model.command.trim() == *command)
            {
                if model.fix_command.trim().is_empty() {
                    model.fix_command = (*fix).to_string();
                    changed = true;
                }
                if model.fix_resume_command.trim().is_empty() {
                    model.fix_resume_command = (*fix_resume).to_string();
                    changed = true;
                }
            }
        }
        if repo_access && self.model_timeout_secs == PRE_BROWSE_TIMEOUT_SECS {
            self.model_timeout_secs = DEFAULT_TIMEOUT_SECS;
            changed = true;
        }
        if self.schema_version < SCHEMA_VERSION {
            self.schema_version = SCHEMA_VERSION;
            changed = true;
        }
        changed
    }

    fn remove_unsafe_coauthors(&mut self) {
        for model in &mut self.models {
            if model.coauthor.trim() == "Antigravity <antigravity@google.com>" {
                model.coauthor.clear();
            }
        }
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
    fn antigravity_does_not_claim_an_unrelated_github_identity() {
        let settings = Settings::default();
        let agy = settings
            .models
            .iter()
            .find(|model| model.name == "agy")
            .unwrap();
        assert!(agy.coauthor.is_empty());
    }

    #[test]
    fn old_antigravity_placeholder_identity_is_removed() {
        let mut settings = Settings::default();
        let agy = settings
            .models
            .iter_mut()
            .find(|model| model.name == "agy")
            .unwrap();
        agy.coauthor = "Antigravity <antigravity@google.com>".into();

        settings.remove_unsafe_coauthors();

        assert!(settings
            .models
            .iter()
            .find(|model| model.name == "agy")
            .unwrap()
            .coauthor
            .is_empty());
    }

    #[test]
    fn migrate_repairs_shipped_defaults_only() {
        let mut s = Settings {
            models: vec![
                ModelConfig {
                    name: "codex".into(),
                    command: "codex exec {prompt}".into(),
                    fix_command: String::new(),
                    coauthor: String::new(),
                    enabled: true,
                    model: String::new(),
                    model_flag: String::new(),
                    effort: String::new(),
                    effort_flag: String::new(),
                    resume_command: String::new(),
                    fix_resume_command: String::new(),
                    session_key: String::new(),
                    price_in: 0.0,
                    price_out: 0.0,
                },
                ModelConfig {
                    name: "mine".into(),
                    command: "mycli --weird {prompt}".into(),
                    fix_command: String::new(),
                    coauthor: String::new(),
                    enabled: true,
                    model: String::new(),
                    model_flag: "-m".into(),
                    effort: String::new(),
                    effort_flag: "-e".into(),
                    resume_command: String::new(),
                    fix_resume_command: String::new(),
                    session_key: String::new(),
                    price_in: 0.0,
                    price_out: 0.0,
                },
            ],
            // a row written before the model/effort/session fields existed
            schema_version: 0,
            ..Settings::default()
        };
        assert!(s.migrate());
        assert_eq!(s.models[0].command, CODEX_CMD);
        assert_eq!(s.models[0].model_flag, "--model");
        // the repaired command also picks up its session wiring
        assert_eq!(s.models[0].session_key, "thread_id");
        assert!(s.models[0].resume_command.contains("resume {session}"));
        // and its starting tier
        assert_eq!(s.models[0].model, CODEX_MODEL);
        assert_eq!(s.models[0].effort, CODEX_EFFORT);
        assert_eq!(s.models[0].effort_flag, "-c");
        // a hand-edited template is left exactly as the user wrote it
        assert_eq!(s.models[1].command, "mycli --weird {prompt}");
        assert_eq!(s.models[1].model_flag, "-m");
        assert!(s.models[1].resume_command.is_empty());
        assert!(s.models[1].model.is_empty());
        assert_eq!(s.models[1].effort_flag, "-e");
        // running it again is a no-op
        assert!(!s.migrate());
    }

    #[test]
    fn an_install_from_before_repo_access_picks_it_up_on_both_templates() {
        let mut s = Settings {
            models: vec![ModelConfig {
                name: "claude".into(),
                command: "claude -p --session-id {session}".into(),
                fix_command: String::new(),
                coauthor: String::new(),
                enabled: true,
                model: "haiku".into(),
                model_flag: "--model".into(),
                effort: "low".into(),
                effort_flag: "--effort".into(),
                resume_command: "claude -p --resume {session}".into(),
                fix_resume_command: String::new(),
                session_key: String::new(),
                price_in: 0.0,
                price_out: 0.0,
            }],
            model_timeout_secs: PRE_BROWSE_TIMEOUT_SECS,
            // already through the model/effort migration, so only the repo
            // access step is outstanding
            schema_version: 1,
            ..Settings::default()
        };
        assert!(s.migrate());
        assert_eq!(s.models[0].command, CLAUDE_CMD);
        // The follow-up resumes through its own template, so it needs the same
        // access as the turn it continues.
        assert_eq!(s.models[0].resume_command, CLAUDE_RESUME);
        assert_eq!(s.model_timeout_secs, DEFAULT_TIMEOUT_SECS);
        // The step that was already taken must not run again.
        assert_eq!(s.models[0].model, "haiku", "the tier fill re-ran");
        assert!(!s.migrate());
    }

    #[test]
    fn a_timeout_the_user_chose_survives_the_upgrade() {
        let mut s = Settings {
            model_timeout_secs: 45,
            schema_version: 1,
            ..Settings::default()
        };
        s.migrate();
        assert_eq!(s.model_timeout_secs, 45);
    }

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
                !model_config.session_key.is_empty() || model_config.command.contains("{session}"),
                "{} can neither report nor accept a session id",
                model_config.name
            );
        }
    }

    #[test]
    fn migrations_restore_read_access_and_add_separate_writable_fix_commands() {
        let mut settings = Settings {
            schema_version: 1,
            model_timeout_secs: 120,
            ..Settings::default()
        };
        let codex = &mut settings.models[1];
        codex.command = "codex exec --skip-git-repo-check --json".into();
        codex.resume_command = "codex exec --skip-git-repo-check --json resume {session} -".into();
        codex.fix_command.clear();
        codex.fix_resume_command.clear();

        assert!(settings.migrate());
        let codex = &settings.models[1];
        assert!(codex.command.contains("--sandbox read-only"));
        assert!(codex.resume_command.contains("--sandbox read-only"));
        assert!(codex.fix_command.contains("--sandbox workspace-write"));
        assert!(codex
            .fix_resume_command
            .contains("--sandbox workspace-write"));
        assert_eq!(settings.model_timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(settings.schema_version, SCHEMA_VERSION);
        assert!(
            !settings.migrate(),
            "a completed migration must be idempotent"
        );
    }

    #[test]
    fn every_default_model_has_a_distinct_writable_fix_path() {
        for model in Settings::default().models {
            assert!(
                !model.fix_command.trim().is_empty(),
                "{} has no fix command",
                model.name
            );
            assert!(
                !model.fix_resume_command.trim().is_empty(),
                "{} has no fix resume command",
                model.name
            );
            assert_ne!(
                model.command, model.fix_command,
                "{} reuses its review command",
                model.name
            );
        }
    }
}
