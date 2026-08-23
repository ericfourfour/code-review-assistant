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

#[cfg(all(test, windows))]
mod tests {
    use super::resolve_program;
    use std::ffi::{OsStr, OsString};

    #[test]
    fn bare_name_resolves_through_pathext() {
        // `cmd` exists on PATH only as `cmd.exe`, so a bare name must find it.
        let resolved = resolve_program(OsStr::new("cmd"));
        let s = resolved.to_string_lossy().to_ascii_lowercase();
        assert!(s.ends_with("cmd.exe"), "unexpected resolution: {s}");
        assert!(s.contains(std::path::MAIN_SEPARATOR), "expected a full path, got {s}");
    }

    #[test]
    fn qualified_and_unknown_names_pass_through() {
        let qualified = OsString::from(r"C:\definitely\not\here.exe");
        assert_eq!(resolve_program(&qualified), qualified);
        let unknown = OsString::from("cra-no-such-program");
        assert_eq!(resolve_program(&unknown), unknown);
    }
}

#[cfg(test)]
mod repo_tests {
    use super::*;
    use crate::testkit::{TempDir, TempRepo};

    /// A Rust file whose added comment says nothing the code does not.
    const LIB_RS: &str = concat!(
        "fn main() {\n",
        "    // Increment the counter by one\n",
        "    counter += 1;\n",
        "}\n",
    );

    #[test]
    fn recognises_a_repo_and_reads_its_branch() {
        let repo = TempRepo::new("detect");
        repo.write("a.txt", "hi\n");
        repo.commit("first");

        assert!(is_git_repo(&repo.path()));
        assert_eq!(current_branch(&repo.path()).unwrap(), "main");
        assert_eq!(repo_name(&repo.path()), repo_name(&repo.path()));
        assert!(!head_sha(&repo.path()).unwrap().is_empty());

        // A plain directory is not a repo, and must not be treated as one.
        let plain = TempDir::new("plain");
        assert!(!is_git_repo(&plain.path().to_string_lossy()));
    }

    #[test]
    fn dirtiness_follows_the_working_tree() {
        let repo = TempRepo::new("dirty");
        repo.write("a.txt", "hi\n");
        repo.commit("first");
        assert!(!is_dirty(&repo.path()), "a clean checkout must not read as dirty");

        repo.write("a.txt", "changed\n");
        assert!(is_dirty(&repo.path()));
        repo.commit("second");
        assert!(!is_dirty(&repo.path()));

        // Untracked files count too — they would be lost by a checkout.
        repo.write("b.txt", "new\n");
        assert!(is_dirty(&repo.path()));
    }

    #[test]
    fn branches_come_back_newest_first_with_their_subjects() {
        let repo = TempRepo::new("branches");
        repo.write("a.txt", "hi\n");
        repo.commit("base commit");
        repo.git(&["checkout", "-b", "feature"]);
        repo.write("a.txt", "more\n");
        repo.commit("feature commit");

        let branches = local_branches(&repo.path()).unwrap();
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, vec!["feature", "main"], "most recently committed first");
        assert_eq!(branches[0].subject, "feature commit");
        assert!(!branches[0].sha.is_empty());
        assert!(!branches[0].age.is_empty());
    }

    #[test]
    fn default_branch_falls_back_when_there_is_no_origin() {
        let repo = TempRepo::new("default");
        repo.write("a.txt", "hi\n");
        repo.commit("first");
        // No remote, so it has to fall back to a branch that exists.
        assert_eq!(default_branch(&repo.path(), "main"), "main");
        // A fallback that does not exist must not be returned blindly.
        assert_eq!(default_branch(&repo.path(), "trunk"), "main");
    }

    #[test]
    fn checkout_switches_branches() {
        let repo = TempRepo::new("checkout");
        repo.write("a.txt", "hi\n");
        repo.commit("first");
        repo.git(&["branch", "feature"]);

        checkout(&repo.path(), "feature").unwrap();
        assert_eq!(current_branch(&repo.path()).unwrap(), "feature");
        assert!(checkout(&repo.path(), "no-such-branch").is_err());
    }

    #[test]
    fn review_diff_covers_working_tree_branch_and_whole_history() {
        let repo = TempRepo::new("diff");
        repo.write("src/lib.rs", "fn main() {}\n");
        repo.commit("base");
        repo.git(&["checkout", "-b", "feature"]);
        repo.write("src/lib.rs", LIB_RS);
        repo.commit("add counter");

        // Against the base branch: only the feature's own change.
        let branch_diff = review_diff(&repo.path(), "main", 12).unwrap();
        assert!(branch_diff.contains("Increment the counter"), "{branch_diff}");

        // Against the empty tree: the whole history, so a brand-new repo has
        // something to review.
        let all = review_diff(&repo.path(), EMPTY_TREE, 12).unwrap();
        assert!(all.contains("src/lib.rs"), "{all}");

        // Empty base means the uncommitted working tree, and a clean tree
        // therefore has nothing to show.
        assert!(review_diff(&repo.path(), "", 12).unwrap().trim().is_empty());
        repo.write("src/lib.rs", LIB_RS.replace("one", "1").as_str());
        assert!(review_diff(&repo.path(), "", 12).unwrap().contains("counter"));
    }

    #[test]
    fn base_label_names_each_kind_of_base() {
        assert_eq!(base_label(""), "HEAD");
        assert_eq!(base_label(EMPTY_TREE), "root");
        assert_eq!(base_label("main"), "main");
    }

    #[test]
    fn stage_and_commit_commits_only_the_named_file() {
        let repo = TempRepo::new("commit");
        repo.write("a.txt", "hi\n");
        repo.write("b.txt", "hi\n");
        repo.commit("first");

        repo.write("a.txt", "edited\n");
        repo.write("b.txt", "also edited\n");
        let sha = stage_and_commit(&repo.path(), "a.txt", "review: tweak a").unwrap();

        assert_eq!(sha, head_sha(&repo.path()).unwrap());
        let last = repo.git(&["log", "-1", "--name-only", "--format=%s"]);
        assert!(last.contains("review: tweak a"), "{last}");
        assert!(last.contains("a.txt"), "{last}");
        assert!(!last.contains("b.txt"), "b.txt should still be uncommitted: {last}");
    }

    /// The whole non-interactive path: take a real diff, find the comment in
    /// it, rewrite that comment on disk, and confirm only those lines moved.
    #[test]
    fn a_real_diff_becomes_an_edit_on_disk() {
        use crate::comments;
        use crate::review::{apply_edit, ReviewFile};

        let repo = TempRepo::new("pipeline");
        repo.write("src/lib.rs", "fn main() {}\n");
        repo.commit("base");
        repo.git(&["checkout", "-b", "feature"]);
        repo.write("src/lib.rs", LIB_RS);
        repo.commit("add counter");

        let diff = review_diff(&repo.path(), "main", 12).unwrap();
        let files = crate::diffparse::parse(&diff);
        let extracted = comments::extract_units(&files, 12);
        assert_eq!(extracted.len(), 1, "one file should have a reviewable comment");
        let (path, units) = &extracted[0];
        assert_eq!(path, "src/lib.rs");
        assert_eq!(units.len(), 1);

        let unit = &units[0];
        assert_eq!(unit.lang, "Rust");
        assert!(unit.has_added);
        let file = ReviewFile { path: path.clone(), units: vec![], line_offset: 0, decided: 0 };
        let replacement = unit.format_replacement("Counting retries, not requests.");
        let delta = apply_edit(&repo.path(), &file, unit, &replacement).unwrap();

        assert_eq!(delta, 0, "one line replaced by one line");
        let after = repo.read("src/lib.rs");
        assert!(after.contains("    // Counting retries, not requests."), "indent lost: {after}");
        assert!(!after.contains("Increment the counter"), "{after}");
        assert!(after.contains("counter += 1;"), "the code itself must be untouched: {after}");
        assert!(is_dirty(&repo.path()), "the edit should show up as a working-tree change");
    }

    /// Guard for the failure mode `apply_edit` exists to prevent: acting on a
    /// diff that no longer matches the file.
    #[test]
    fn an_edit_against_a_changed_file_is_refused() {
        use crate::comments;
        use crate::review::{apply_edit, ReviewFile};

        let repo = TempRepo::new("stale");
        repo.write("src/lib.rs", "fn main() {}\n");
        repo.commit("base");
        repo.git(&["checkout", "-b", "feature"]);
        repo.write("src/lib.rs", LIB_RS);
        repo.commit("add counter");

        let diff = review_diff(&repo.path(), "main", 12).unwrap();
        let extracted = comments::extract_units(&crate::diffparse::parse(&diff), 12);
        let (path, units) = &extracted[0];
        let file = ReviewFile { path: path.clone(), units: vec![], line_offset: 0, decided: 0 };

        // Someone edits the file behind our back.
        repo.write("src/lib.rs", "fn main() {\n    counter += 1;\n}\n");
        let err = apply_edit(&repo.path(), &file, &units[0], &[]).unwrap_err();
        assert!(
            err.contains("mismatch") || err.contains("out of bounds"),
            "should refuse the stale edit, said: {err}"
        );
    }
}
