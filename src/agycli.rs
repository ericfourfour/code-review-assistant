//! The agy CLI's configuration, owned by this app rather than shared.
//!
//! agy takes its permissions from a settings file in its home directory, and
//! there is no flag to grant one per invocation. Writing the user's own
//! `~/.gemini` would silently re-govern every agy session on the machine —
//! read-only, no URLs, scoped to whichever repository was reviewed last. So
//! the reviewer is given a home of its own with `--gemini_dir`, and the rules
//! below apply to it alone. Authentication is unaffected: agy reads it from
//! the OS keyring, not from this directory.
//!
//! The rules say what a reviewer needs and nothing else: read this repository,
//! run shell commands only inside the sandbox, write nothing, reach no URLs.
//! Today the sandbox does not engage in print mode — it wants an admin
//! escalation no one can answer — so `unsandboxed(*)` denies every command in
//! practice. That is the intended reading either way, and it degrades well: a
//! refused command is a rule the model can route around and still answer,
//! where an unanswerable confirmation prompt ends the whole run.

use std::path::PathBuf;

/// Directory handed to agy as its home. Kept beside the database rather than
/// in the repository: it is app state, not project state.
pub fn home_dir() -> PathBuf {
    let mut dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("code-review-assistant");
    dir.push("agy-home");
    dir
}

/// The permission rules, as agy's settings file spells them.
fn settings_json(repo: &str) -> String {
    // Forward slashes: the file is JSON, and a Windows path with backslashes
    // would need escaping that agy does not undo when matching.
    let repo = repo.replace('\\', "/");
    format!(
        "{{\n  \"enableTerminalSandbox\": true,\n  \"permissions\": {{\n    \
\"allow\": [\n      \"command(*)\",\n      \"read_file({repo})\"\n    ],\n    \
\"deny\": [\n      \"unsandboxed(*)\",\n      \"write_file(*)\",\n      \
\"read_url(*)\",\n      \"execute_url(*)\"\n    ]\n  }}\n}}\n"
    )
}

/// Fix sessions get a separate home whose write grant is scoped to the same
/// repository. Keeping it separate prevents a concurrent reviewer from ever
/// inheriting writable permissions.
fn fix_settings_json(repo: &str) -> String {
    let repo = repo.replace('\\', "/");
    format!(
        "{{\n  \"enableTerminalSandbox\": true,\n  \"permissions\": {{\n    \
\"allow\": [\n      \"command(*)\",\n      \"read_file({repo})\",\n      \
\"write_file({repo})\"\n    ],\n    \"deny\": [\n      \"unsandboxed(*)\",\n      \
\"read_url(*)\",\n      \"execute_url(*)\"\n    ]\n  }}\n}}\n"
    )
}

/// Point agy's home at `repo` and return the directory to pass it.
///
/// Rewritten whenever the repository changes, because the read grant names one
/// path. Cheap enough to do per review: it is one small file, and skipping the
/// write when nothing changed keeps it off the disk on every comment.
pub fn configure(repo: &str) -> Result<PathBuf, String> {
    configure_home(home_dir(), settings_json(repo))
}

pub fn configure_fix(repo: &str) -> Result<PathBuf, String> {
    let mut home = home_dir();
    home.set_file_name("agy-fix-home");
    configure_home(home, fix_settings_json(repo))
}

fn configure_home(home: PathBuf, wanted: String) -> Result<PathBuf, String> {
    let dir = home.join("antigravity-cli");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join("settings.json");
    if std::fs::read_to_string(&path)
        .map(|c| c == wanted)
        .unwrap_or(false)
    {
        return Ok(home);
    }
    std::fs::write(&path, &wanted).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_grant_this_repo_and_nothing_wider() {
        let json = settings_json("C:/work/widgets");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let allow = v["permissions"]["allow"].as_array().unwrap();
        let deny = v["permissions"]["deny"].as_array().unwrap();
        assert!(
            allow.iter().any(|r| r == "read_file(C:/work/widgets)"),
            "{allow:?}"
        );
        // Reading anywhere would let a reviewer wander out of the repository
        // it was asked about.
        assert!(!allow.iter().any(|r| r == "read_file(*)"), "{allow:?}");
        // Commands are allowed only in the sandbox, and writes and the network
        // not at all — the edits are the app's to apply.
        assert!(deny.iter().any(|r| r == "unsandboxed(*)"), "{deny:?}");
        assert!(deny.iter().any(|r| r == "write_file(*)"), "{deny:?}");
        assert!(deny.iter().any(|r| r == "read_url(*)"), "{deny:?}");
        assert_eq!(v["enableTerminalSandbox"], serde_json::Value::Bool(true));
    }

    #[test]
    fn fix_rules_grant_writes_only_inside_the_repository() {
        let json = fix_settings_json("C:/work/widgets");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let allow = v["permissions"]["allow"].as_array().unwrap();
        let deny = v["permissions"]["deny"].as_array().unwrap();
        assert!(allow.iter().any(|r| r == "write_file(C:/work/widgets)"));
        assert!(!allow.iter().any(|r| r == "write_file(*)"));
        assert!(deny.iter().any(|r| r == "unsandboxed(*)"));
    }

    #[test]
    fn a_windows_path_is_written_with_forward_slashes() {
        let json = settings_json("C:\\work\\widgets");
        assert!(json.contains("read_file(C:/work/widgets)"), "{json}");
        // Backslashes would have to survive JSON escaping and agy's own
        // matching; sidestep both.
        assert!(!json.contains('\\'), "{json}");
    }

    #[test]
    fn configuring_writes_the_file_and_follows_the_repository() {
        let tmp = crate::testkit::TempDir::new("agyhome");
        let dir = tmp.path().join("antigravity-cli");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        // Written for one repo, then rewritten when the review moves to
        // another — the read grant names a single path.
        std::fs::write(&path, settings_json("C:/work/a")).unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("read_file(C:/work/a)"));
        std::fs::write(&path, settings_json("C:/work/b")).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("read_file(C:/work/b)"));
        assert!(!after.contains("read_file(C:/work/a)"));
    }

    #[test]
    fn the_home_is_app_state_not_project_state() {
        let home = home_dir();
        assert!(home.ends_with("agy-home"), "{}", home.display());
        assert!(
            home.to_string_lossy().contains("code-review-assistant"),
            "{}",
            home.display()
        );
    }
}
