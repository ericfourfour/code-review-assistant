//! App settings, persisted as JSON in the sqlite `settings` table.

use serde::{Deserialize, Serialize};

use crate::db::Db;

/// One reviewer model backed by an installed, authenticated CLI.
/// `command` is a whitespace-tokenized template; the `{prompt}` token is
/// replaced with the prompt text. If no `{prompt}` token is present the
/// prompt is piped to the process on stdin. No shell is involved.
#[derive(Clone, Serialize, Deserialize)]
pub struct ModelSlot {
    pub name: String,
    pub command: String,
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
    /// so follow-ups are unavailable for the slot.
    #[serde(default)]
    pub resume_command: String,
    /// JSON key in the CLI output that carries the session id. Empty means
    /// the id is ours to generate — `command` must then contain `{session}`
    /// (that is claude's `--session-id <uuid>`).
    #[serde(default)]
    pub session_key: String,
}

fn default_model_flag() -> String {
    "--model".into()
}

fn default_effort_flag() -> String {
    "--effort".into()
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
    pub models: Vec<ModelSlot>,
    /// Fallback base branch when origin/HEAD cannot be resolved.
    pub default_base: String,
    /// Command used for PR listing/checkout (normally `gh`).
    pub gh_path: String,
    /// Seconds before a model CLI call is abandoned.
    pub model_timeout_secs: u64,
    /// Context lines shown around a comment in the prompt and the UI.
    pub context_lines: usize,
    pub recent_repos: Vec<String>,
    /// Bumped when a migration step must run exactly once. Absent (0) in rows
    /// written before this field existed.
    #[serde(default)]
    pub schema_version: u32,
}

/// Current settings schema. Bump when adding a one-shot migration step.
const SCHEMA_VERSION: u32 = 1;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            models: vec![
                ModelSlot {
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
                },
                ModelSlot {
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
                },
                // agy has no stdin mode, but it is a native .exe, so the
                // prompt goes as an argument (32k limit, not cmd.exe's 8k).
                ModelSlot {
                    name: "agy".into(),
                    command: AGY_CMD.into(),
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
                    session_key: "conversation_id".into(),
                },
            ],
            default_base: "main".into(),
            gh_path: "gh".into(),
            model_timeout_secs: 120,
            context_lines: 12,
            recent_repos: Vec::new(),
            schema_version: SCHEMA_VERSION,
        }
    }
}

const KEY: &str = "settings";

// The shipped command for each CLI, and the command that continues its
// conversation. Each first call asks for machine-readable output so the
// session id can be recovered — except claude, which lets us name the id.
const CLAUDE_CMD: &str = "claude -p --session-id {session}";
const CLAUDE_RESUME: &str = "claude -p --resume {session}";
const CODEX_CMD: &str = "codex exec --skip-git-repo-check --json";
const CODEX_RESUME: &str = "codex exec --skip-git-repo-check --json resume {session} -";
const AGY_CMD: &str = "agy -p {prompt} --output-format json";
const AGY_RESUME: &str = "agy -p {prompt} --output-format json --conversation {session}";

// Start small and cheap: a first-pass comment reviewer runs on every hunk, and
// the cheapest tier of each family handles "does this comment restate the
// code" perfectly well. Raise per slot in settings when a repo needs it.
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
/// one. Applied on load so an existing install picks up fixes — and session
/// support — without the user having to notice and edit settings by hand.
const COMMAND_FIXUPS: &[(&str, &str)] = &[
    // Multi-line arguments are rejected outright for `.cmd` shims on Windows
    // ("batch file arguments are invalid"), so codex has to take stdin.
    ("codex exec {prompt}", CODEX_CMD),
    ("codex exec --skip-git-repo-check", CODEX_CMD),
    ("claude -p {prompt}", CLAUDE_CMD),
    ("claude -p", CLAUDE_CMD),
    ("agy -p {prompt}", AGY_CMD),
    ("agy --print -", AGY_CMD),
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

    /// Repair known-broken command templates. Only exact matches against a
    /// shipped default are touched — a template the user has edited is theirs.
    fn migrate(&mut self) -> bool {
        let mut changed = false;
        let fresh_fields = self.schema_version < SCHEMA_VERSION;
        for m in &mut self.models {
            if let Some((_, fixed)) =
                COMMAND_FIXUPS.iter().find(|(broken, _)| m.command.trim() == *broken)
            {
                m.command = (*fixed).to_string();
                changed = true;
            }
            if m.model_flag.trim().is_empty() {
                m.model_flag = default_model_flag();
                changed = true;
            }
            if m.effort_flag.trim().is_empty() {
                m.effort_flag = default_effort_flag();
                changed = true;
            }
            // Give a recognised command its starting model/effort, but only
            // on the one pass that upgrades the row — after that an empty
            // model means the user chose the CLI default, and we leave it be.
            if fresh_fields {
                if let Some((_, model, effort, effort_flag)) =
                    STARTING_TIER.iter().find(|(cmd, ..)| m.command.trim() == *cmd)
                {
                    if m.model.trim().is_empty() {
                        m.model = (*model).to_string();
                    }
                    if m.effort.trim().is_empty() {
                        m.effort = (*effort).to_string();
                        m.effort_flag = (*effort_flag).to_string();
                    }
                    changed = true;
                }
            }
            // Fill in session wiring for a recognised command that predates it.
            if m.resume_command.trim().is_empty() {
                if let Some((_, resume, key)) =
                    SESSION_WIRING.iter().find(|(cmd, _, _)| m.command.trim() == *cmd)
                {
                    m.resume_command = (*resume).to_string();
                    m.session_key = (*key).to_string();
                    changed = true;
                }
            }
        }
        if fresh_fields {
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

    pub fn enabled_models(&self) -> Vec<(usize, ModelSlot)> {
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
}
