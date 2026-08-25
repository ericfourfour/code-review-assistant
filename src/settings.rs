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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            models: vec![
                ModelSlot {
                    name: "claude".into(),
                    command: "claude -p {prompt}".into(),
                    coauthor: "Claude <noreply@anthropic.com>".into(),
                    enabled: true,
                },
                ModelSlot {
                    name: "codex".into(),
                    command: "codex exec {prompt}".into(),
                    coauthor: "Codex <codex@openai.com>".into(),
                    enabled: true,
                },
                ModelSlot {
                    name: "agy".into(),
                    command: "agy -p {prompt}".into(),
                    // Google does not publish a GitHub co-author identity for
                    // Antigravity. The old antigravity@google.com placeholder
                    // is associated with an unrelated GitHub user, so rely on
                    // the agy provenance trailer instead.
                    coauthor: String::new(),
                    enabled: true,
                },
            ],
            default_base: "main".into(),
            gh_path: "gh".into(),
            model_timeout_secs: 120,
            context_lines: 12,
            recent_repos: Vec::new(),
        }
    }
}

const KEY: &str = "settings";

impl Settings {
    pub fn load(db: &Db) -> Settings {
        let mut settings: Settings = db
            .get_setting(KEY)
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        settings.remove_unsafe_coauthors();
        settings
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
