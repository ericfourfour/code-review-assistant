//! Git and GitHub-CLI plumbing. Everything shells out to `git` / `gh` so the
//! app works against whatever the user already has installed and
//! authenticated.

use serde::Deserialize;
use std::path::Path;
use std::process::Command;

pub fn run(dir: &str, program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            if stderr.trim().is_empty() { stdout } else { stderr }
        ))
    }
}

fn git(dir: &str, args: &[&str]) -> Result<String, String> {
    run(dir, "git", args)
}

pub fn is_git_repo(path: &str) -> bool {
    Path::new(path).is_dir()
        && Command::new("git")
            .args(["-C", path, "rev-parse", "--git-dir"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

pub fn repo_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

pub fn current_branch(dir: &str) -> Result<String, String> {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|s| s.trim().to_string())
}

pub fn head_sha(dir: &str) -> Result<String, String> {
    git(dir, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string())
}

pub fn is_dirty(dir: &str) -> bool {
    git(dir, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Resolve the repo's default branch: origin/HEAD if set, else the fallback.
pub fn default_branch(dir: &str, fallback: &str) -> String {
    if let Ok(s) = git(dir, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        if let Some(b) = s.trim().strip_prefix("origin/") {
            return b.to_string();
        }
    }
    for cand in [fallback, "main", "master"] {
        if git(dir, &["rev-parse", "--verify", &format!("refs/heads/{cand}")]).is_ok() {
            return cand.to_string();
        }
    }
    fallback.to_string()
}

#[derive(Clone)]
pub struct BranchInfo {
    pub name: String,
    pub sha: String,
    pub age: String,
    pub subject: String,
}

pub fn local_branches(dir: &str) -> Result<Vec<BranchInfo>, String> {
    let out = git(
        dir,
        &[
            "for-each-ref",
            "refs/heads",
            "--sort=-committerdate",
            "--format=%(refname:short)\t%(objectname:short)\t%(committerdate:relative)\t%(contents:subject)",
        ],
    )?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(4, '\t');
            Some(BranchInfo {
                name: it.next()?.to_string(),
                sha: it.next().unwrap_or("").to_string(),
                age: it.next().unwrap_or("").to_string(),
                subject: it.next().unwrap_or("").to_string(),
            })
        })
        .collect())
}

pub fn checkout(dir: &str, branch: &str) -> Result<(), String> {
    git(dir, &["checkout", branch]).map(|_| ())
}

/// Diff of `base...HEAD` (merge-base) with generous context. When `base` is
/// empty, diff the working tree against HEAD instead.
pub fn review_diff(dir: &str, base: &str, context: usize) -> Result<String, String> {
    let u = format!("-U{context}");
    if base.is_empty() {
        git(dir, &["diff", "--no-color", &u, "HEAD"])
    } else {
        git(dir, &["diff", "--no-color", &u, &format!("{base}...HEAD")])
    }
}

pub fn stage_and_commit(dir: &str, file: &str, message: &str) -> Result<String, String> {
    git(dir, &["add", "--", file])?;
    git(dir, &["commit", "-m", message, "--", file])?;
    head_sha(dir)
}

// ---------------------------------------------------------------------------
// GitHub PRs via the `gh` CLI

#[derive(Clone, Deserialize)]
pub struct PrAuthor {
    #[serde(default)]
    pub login: String,
}

#[derive(Clone, Deserialize)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    #[serde(rename = "headRefName")]
    pub head_ref: String,
    #[serde(rename = "baseRefName")]
    pub base_ref: String,
    #[serde(default)]
    pub author: Option<PrAuthor>,
}

pub fn open_prs(dir: &str, gh: &str) -> Result<Vec<PrInfo>, String> {
    let out = run(
        dir,
        gh,
        &[
            "pr", "list", "--state", "open", "--limit", "50", "--json",
            "number,title,headRefName,baseRefName,author",
        ],
    )?;
    serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))
}

pub fn pr_checkout(dir: &str, gh: &str, number: u64) -> Result<(), String> {
    run(dir, gh, &["pr", "checkout", &number.to_string()]).map(|_| ())
}
