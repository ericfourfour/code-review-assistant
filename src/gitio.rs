//! Git and GitHub-CLI plumbing. Everything shells out to `git` / `gh` so the
//! app works against whatever the user already has installed and
//! authenticated.

use serde::Deserialize;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// Build a `Command` that will not flash a console window on Windows. The
/// app is a windowed (no-console) process, so console-subsystem children
/// (git, gh, model CLIs) would otherwise each open their own terminal.
pub fn hidden_command<S: AsRef<std::ffi::OsStr>>(program: S) -> Command {
    #[cfg(windows)]
    let program = resolve_program(program.as_ref());
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

/// Look a bare program name up on `PATH` the way `cmd.exe` would.
///
/// `Command::new("codex")` only ever appends `.exe`, so npm-style `codex.cmd`
/// shims fail to spawn with "program not found" even though the CLI is
/// installed. Resolving to a full path here also lets std recognise a
/// `.cmd`/`.bat` target and route it through `cmd.exe` with its own quoting.
#[cfg(windows)]
fn resolve_program(program: &std::ffi::OsStr) -> std::ffi::OsString {
    use std::path::{Path, PathBuf};

    // Anything that already carries a directory is used verbatim.
    let p = Path::new(program);
    if p.components().count() > 1 {
        return program.to_os_string();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return program.to_os_string();
    };
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let exts: Vec<&str> = pathext.split(';').filter(|e| !e.is_empty()).collect();
    let spelled_out = p.extension().is_some();

    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let base = dir.join(program);
        // An explicit extension wins over PATHEXT, as in cmd.exe.
        if spelled_out && base.is_file() {
            return base.into_os_string();
        }
        for ext in &exts {
            let mut cand = base.clone().into_os_string();
            cand.push(ext);
            if PathBuf::from(&cand).is_file() {
                return cand;
            }
        }
    }
    program.to_os_string()
}

pub fn run(dir: &str, program: &str, args: &[&str]) -> Result<String, String> {
    let out = hidden_command(program)
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
            if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            }
        ))
    }
}

fn git(dir: &str, args: &[&str]) -> Result<String, String> {
    run(dir, "git", args)
}

pub fn is_git_repo(path: &str) -> bool {
    Path::new(path).is_dir()
        && hidden_command("git")
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
    if let Ok(s) = git(
        dir,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        if let Some(b) = s.trim().strip_prefix("origin/") {
            return b.to_string();
        }
    }
    for cand in [fallback, "main", "master"] {
        if git(
            dir,
            &["rev-parse", "--verify", &format!("refs/heads/{cand}")],
        )
        .is_ok()
        {
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

/// Git's well-known empty-tree object. Used as the diff base when a branch
/// has nothing to be compared against (e.g. the default branch of a brand-new
/// repo), so the whole history becomes reviewable.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Human-readable name for a diff base, for the UI and session log.
pub fn base_label(base: &str) -> &str {
    match base {
        "" => "HEAD",
        EMPTY_TREE => "root",
        other => other,
    }
}

/// Diff of `base...HEAD` (merge-base) with generous context. When `base` is
/// empty, diff the working tree against HEAD instead; when it is
/// [`EMPTY_TREE`], diff the entire history.
pub fn review_diff(dir: &str, base: &str, context: usize) -> Result<String, String> {
    let u = format!("-U{context}");
    if base.is_empty() {
        git(dir, &["diff", "--no-color", &u, "HEAD"])
    } else if base == EMPTY_TREE {
        // A tree object has no merge base, so use a plain two-point diff.
        git(dir, &["diff", "--no-color", &u, EMPTY_TREE, "HEAD"])
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
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            "50",
            "--json",
            "number,title,headRefName,baseRefName,author",
        ],
    )?;
    serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))
}

pub fn pr_checkout(dir: &str, gh: &str, number: u64) -> Result<(), String> {
    run(dir, gh, &["pr", "checkout", &number.to_string()]).map(|_| ())
}
