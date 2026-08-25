//! Git and GitHub CLI process execution. Everything runs `git` / `gh` so the
//! app works against whatever the user already has installed and
//! authenticated.

use serde::Deserialize;
use std::io::Write;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

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

/// Whether one file has uncommitted state — staged, unstaged, or untracked.
/// Pathspec-scoped, so an unrelated dirty file cannot answer for this one.
pub fn file_is_dirty(dir: &str, file: &str) -> bool {
    git(dir, &["status", "--porcelain", "--", file])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Whether the index contains any change relative to HEAD.
pub fn index_is_dirty(dir: &str) -> bool {
    git(dir, &["diff", "--cached", "--name-only"])
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

/// The ref a pull request's base should actually be diffed against.
///
/// `gh pr checkout` fetches the pull request's head and nothing else, so the
/// *local* branch named by `baseRefName` is only as current as whatever the
/// reviewer last pulled. Diffing `base...HEAD` against a stale tip puts the
/// merge base too far back, and every change merged into the base since then
/// arrives in the review dressed as part of the pull request. The
/// remote-tracking ref is what the pull request is genuinely proposed
/// against, so refresh it and prefer it — falling back to the plain name when
/// there is no such ref to prefer (no remote, a fork's base, offline).
pub fn pr_base_ref(dir: &str, base: &str) -> String {
    let _ = git(dir, &["fetch", "--quiet", "origin", base]);
    let remote = format!("origin/{base}");
    match git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/{remote}"),
        ],
    ) {
        Ok(_) => remote,
        Err(_) => base.to_string(),
    }
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

/// Move HEAD onto `rev` without claiming a branch. A review only ever reads
/// `base...HEAD`, so a detached HEAD is exactly as reviewable as an attached
/// one — and it is the only way in when the branch is checked out elsewhere.
pub fn checkout_detached(dir: &str, rev: &str) -> Result<(), String> {
    git(dir, &["checkout", "--detach", rev]).map(|_| ())
}

/// Path of the worktree that holds `branch`, ignoring the one at `except`.
/// Git gives a branch to one worktree at a time, so this is what turns a
/// checkout into a refusal — and what tells the caller to detach instead.
/// `except` is the worktree doing the asking, which its own branch never
/// blocks: it is already standing on it.
pub fn worktree_for_branch(dir: &str, branch: &str, except: &str) -> Option<String> {
    let out = git(dir, &["worktree", "list", "--porcelain"]).ok()?;
    let want = format!("refs/heads/{branch}");
    let mut path = String::new();
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = p.trim().to_string();
        } else if line.strip_prefix("branch ").map(str::trim) == Some(want.as_str())
            && !path.is_empty()
            && !same_path(&path, except)
        {
            return Some(path);
        }
    }
    None
}

/// Create a linked worktree at `path` with `target` checked out in it.
pub fn worktree_add(dir: &str, path: &str, target: &str) -> Result<(), String> {
    git(dir, &["worktree", "add", path, target]).map(|_| ())
}

/// The same, with HEAD detached — for a target some other worktree already
/// holds, and for a starting point that is about to be moved anyway.
pub fn worktree_add_detached(dir: &str, path: &str, target: &str) -> Result<(), String> {
    git(dir, &["worktree", "add", "--detach", path, target]).map(|_| ())
}

/// Forget worktrees whose directories are gone. Git keeps the registration
/// after a directory is deleted, and the stale entry refuses the path back.
pub fn worktree_prune(dir: &str) {
    let _ = git(dir, &["worktree", "prune"]);
}

/// Whether two spellings name the same path.
pub fn same_path(a: &str, b: &str) -> bool {
    path_key(a) == path_key(b)
}

/// One spelling of a path, so two of them can be compared or one can name a
/// directory: git reports worktrees with forward slashes, and Windows does
/// not care about case.
pub fn path_key(p: &str) -> String {
    let s = p.trim().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// Git's well-known empty-tree object. Used as the diff base when a branch
/// has nothing to be compared against (e.g. the default branch of a brand-new
/// repo), so the whole history becomes reviewable.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Sentinel base for "review what is staged": `git diff --cached`. The colon
/// makes it impossible as a ref name, so it can never shadow a real branch.
pub const STAGED: &str = ":staged";

/// Sentinel base for the working-tree diff with untracked files appended as
/// new-file hunks — `git diff HEAD` alone never shows a file git is not
/// tracking, so a brand-new module would silently escape review.
pub const UNTRACKED: &str = ":worktree+untracked";

/// Human-readable name for a diff base, for the UI and session log.
pub fn base_label(base: &str) -> &str {
    match base {
        "" => "HEAD",
        EMPTY_TREE => "root",
        STAGED => "staged",
        UNTRACKED => "HEAD+untracked",
        other => other,
    }
}

/// Inverse of [`base_label`], so a plan (which stores the label) can re-run
/// the diff it was built from.
// The expanded-workflows layer consumes this helper. Keep this stack layer
// independently lint-clean before that caller is introduced.
#[allow(dead_code)]
pub fn base_from_label(label: &str) -> String {
    match label {
        "HEAD" => String::new(),
        "root" => EMPTY_TREE.to_string(),
        "staged" => STAGED.to_string(),
        "HEAD+untracked" => UNTRACKED.to_string(),
        other => other.to_string(),
    }
}

/// Diff of `base...HEAD` (merge-base) with generous context. When `base` is
/// empty, diff the working tree against HEAD instead; when it is
/// [`EMPTY_TREE`], diff the entire history; [`STAGED`] diffs the index, and
/// [`UNTRACKED`] is the working-tree diff plus every untracked file.
pub fn review_diff(dir: &str, base: &str, context: usize) -> Result<String, String> {
    let u = format!("-U{context}");
    if base.is_empty() {
        git(dir, &["diff", "--no-color", &u, "HEAD"])
    } else if base == STAGED {
        git(dir, &["diff", "--no-color", &u, "--cached"])
    } else if base == UNTRACKED {
        let mut diff = git(dir, &["diff", "--no-color", &u, "HEAD"])?;
        for path in untracked_files(dir)? {
            if let Some(d) = untracked_file_diff(dir, &path, context) {
                diff.push_str(&d);
            }
        }
        Ok(diff)
    } else if base == EMPTY_TREE {
        // A tree object has no merge base, so use a plain two-point diff.
        git(dir, &["diff", "--no-color", &u, EMPTY_TREE, "HEAD"])
    } else {
        git(dir, &["diff", "--no-color", &u, &format!("{base}...HEAD")])
    }
}

/// Files in the working tree that git does not track, honouring .gitignore.
/// `-z` keeps unusual path characters literal instead of quoted-and-escaped.
pub fn untracked_files(dir: &str) -> Result<Vec<String>, String> {
    let out = git(dir, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    Ok(out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// One untracked file rendered as a new-file diff. `--no-index` against
/// git's special `/dev/null` name works on every platform, but exits 1 when
/// the sides differ — which here means success, so [`run`] cannot be used.
/// A file that cannot be diffed (unreadable, binary emits no hunks anyway)
/// yields None and is simply left out, degraded rather than fatal.
fn untracked_file_diff(dir: &str, path: &str, context: usize) -> Option<String> {
    let u = format!("-U{context}");
    let out = hidden_command("git")
        .args([
            "diff",
            "--no-color",
            &u,
            "--no-index",
            "--",
            "/dev/null",
            path,
        ])
        .current_dir(dir)
        .output()
        .ok()?;
    match out.status.code() {
        Some(0) | Some(1) => Some(String::from_utf8_lossy(&out.stdout).to_string()),
        _ => None,
    }
}

/// Which tree a [`review_diff`]'s *new side* describes. Every ranged diff
/// (branch, PR, whole history) ends at HEAD; only the bare working-tree diff
/// shows uncommitted lines. Extraction must read file content from the same
/// tree the diff's line numbers point into, or a single uncommitted edit
/// above a hunk shifts every line and semantic context silently degrades.
#[derive(Clone, Copy, PartialEq)]
pub enum NewSide {
    WorkTree,
    Head,
    /// The staged diff ends at the index — not the worktree (unstaged edits
    /// would shift line numbers) and not HEAD (the staged lines are not there).
    Index,
}

/// The new side of the diff [`review_diff`] produces for this base.
pub fn new_side(base: &str) -> NewSide {
    if base.is_empty() || base == UNTRACKED {
        NewSide::WorkTree
    } else if base == STAGED {
        NewSide::Index
    } else {
        NewSide::Head
    }
}

/// A file's content as HEAD has it. `None` when HEAD holds no such file,
/// which callers treat like any unreadable file: hunk context, no semantic
/// split — degraded, never wrong.
pub fn file_at_head(dir: &str, path: &str) -> Option<String> {
    git(dir, &["show", &format!("HEAD:{path}")]).ok()
}

/// A file's content as the index has it (stage 0). Same degradation contract
/// as [`file_at_head`].
pub fn file_at_index(dir: &str, path: &str) -> Option<String> {
    git(dir, &["show", &format!(":0:{path}")]).ok()
}

/// Replace one stage-0 index blob without touching the working tree. The file
/// mode is preserved from the existing index entry.
pub fn write_index_file(dir: &str, path: &str, content: &str) -> Result<(), String> {
    let entry = git(dir, &["ls-files", "-s", "--", path])?;
    let mode = entry
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("no staged index entry for {path}"))?
        .to_string();

    let mut child = hidden_command("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git hash-object: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "git hash-object stdin unavailable".to_string())?
        .write_all(content.as_bytes())
        .map_err(|e| format!("git hash-object stdin: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git hash-object: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let object = String::from_utf8_lossy(&out.stdout).trim().to_string();
    git(
        dir,
        &["update-index", "--add", "--cacheinfo", &mode, &object, path],
    )?;
    Ok(())
}

pub fn stage_and_commit(dir: &str, file: &str, message: &str) -> Result<String, String> {
    git(dir, &["add", "--", file])?;
    git(dir, &["commit", "-m", message, "--", file])?;
    head_sha(dir)
}

/// Commit the index exactly as it stands. Unlike [`stage_and_commit`], this
/// never copies an unstaged working-tree version over a reviewed staged blob.
pub fn commit_index(dir: &str, message: &str) -> Result<String, String> {
    git(dir, &["commit", "-m", message])?;
    head_sha(dir)
}

// ---------------------------------------------------------------------------
// Delivering a finished review

/// Where a finished review's commits can go: the branch they were made on,
/// the remote that branch belongs to, and how far the local copy has run
/// ahead of what the remote holds.
///
/// Everything here is read-only and best-effort — a repository with no
/// remotes, a detached HEAD and a branch nobody has ever pushed are all
/// ordinary states, and each one only rules out some of the routes out.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Delivery {
    /// The branch HEAD is on. `None` on a detached HEAD, which is what a
    /// review gets when the branch was already checked out somewhere else —
    /// the commits are real, but they are on no branch.
    pub branch: Option<String>,
    /// The remote to publish to: the branch's own, else `origin`, else the
    /// only one configured. `None` when the repository has no remotes at all.
    pub remote: Option<String>,
    /// The remote-tracking ref the local commits were compared against, e.g.
    /// `origin/feature`. `None` when the branch has never been pushed.
    pub upstream: Option<String>,
    /// The commit that will actually be published — normally `HEAD`.
    ///
    /// It is a sha instead when the review's own commits are not on this
    /// checkout, which is what a detached review looks like once its commits
    /// have been rescued onto a branch of their own: the session made them,
    /// they are real, and HEAD has moved back to where the remote is. Asking
    /// HEAD what there is to publish answers "nothing" over the top of a
    /// summary that just said one commit was made.
    pub tip: String,
    /// The branch those commits live on, when they are not on this checkout.
    pub tip_branch: Option<String>,
    /// Commits publishing would add, newest first — everything between the
    /// comparison point and HEAD.
    pub unpushed: Vec<String>,
    /// Commits the remote has that HEAD does not. Any at all means a plain
    /// push is a non-fast-forward and will be refused.
    pub behind: usize,
    /// Uncommitted changes in the review's checkout — work neither route
    /// would publish, so the reviewer is told before they pick one.
    pub dirty: bool,
}

impl Delivery {
    pub fn ahead(&self) -> usize {
        self.unpushed.len()
    }

    /// Whether a plain push of these commits can work at all: somewhere to
    /// push to, and something to send.
    pub fn can_push(&self) -> bool {
        self.remote.is_some() && !self.unpushed.is_empty()
    }
}

/// Read the delivery state of `dir`, for a review of `ref_name` against
/// `base`, whose session made `session_commits`.
///
/// `ref_name` is what the branch is called on the remote — needed because a
/// detached review HEAD cannot name itself — and `base` is the fallback
/// comparison point for a branch the remote has never seen, where "what a
/// push would add" is the whole branch. `session_commits` is what keeps the
/// answer about the *review* rather than about the checkout: see
/// [`Delivery::tip`].
#[cfg(test)]
pub fn delivery_state(
    dir: &str,
    ref_name: &str,
    base: &str,
    session_commits: &[String],
) -> Delivery {
    delivery_state_for_remote(dir, ref_name, base, session_commits, None)
}

/// [`delivery_state`] pinned to one remote. PR heads use this because a
/// detached fork checkout cannot reveal its repository through a local branch
/// upstream, and falling back to `origin/<short-name>` can identify a wholly
/// different branch in the base repository.
pub fn delivery_state_for_remote(
    dir: &str,
    ref_name: &str,
    base: &str,
    session_commits: &[String],
    preferred_remote: Option<&str>,
) -> Delivery {
    let branch = current_branch(dir)
        .ok()
        .filter(|b| b != "HEAD" && !b.is_empty());
    let remote = preferred_remote
        .filter(|r| !r.trim().is_empty())
        .map(str::to_string)
        .or_else(|| default_remote(dir, branch.as_deref()));
    // The branch's configured upstream first — it is the reviewer's own answer
    // to where this branch goes. Failing that, the tracking ref for the name
    // the review was started under, which is what a detached PR checkout has.
    let configured = branch.as_deref().and_then(|b| upstream_ref(dir, b));
    let upstream = preferred_remote
        .and_then(|wanted| {
            configured
                .as_ref()
                .filter(|u| u.starts_with(&format!("{wanted}/")))
                .cloned()
        })
        .or_else(|| {
            let cand = format!("{}/{ref_name}", remote.as_deref()?);
            has_ref(dir, &format!("refs/remotes/{cand}")).then_some(cand)
        })
        .or_else(|| preferred_remote.is_none().then_some(configured).flatten());

    // Publish what the review committed. Only when HEAD cannot reach those
    // commits does the tip move off it — otherwise this is HEAD, and every
    // ordinary review takes the cheap path.
    let stranded: Vec<String> = session_commits
        .iter()
        .filter(|sha| exists(dir, sha) && !is_ancestor(dir, sha, "HEAD"))
        .cloned()
        .collect();
    let (tip, tip_branch) = match newest(dir, &stranded) {
        Some(sha) => {
            let on = branch_containing(dir, &sha);
            (sha, on)
        }
        None => ("HEAD".to_string(), None),
    };

    let (behind, unpushed) = match &upstream {
        Some(u) => (
            commits_between(dir, &tip, u).len(),
            commits_between(dir, u, &tip),
        ),
        // Never pushed: everything the review was scoped to is unpublished.
        None => (0, commits_between(dir, base, &tip)),
    };
    Delivery {
        branch,
        remote,
        upstream,
        tip,
        tip_branch,
        unpushed,
        behind,
        dirty: is_dirty(dir),
    }
}

/// Whether this repository still has `rev` — a commit left on no branch can
/// be collected, and asking about one that is gone is an error, not a fact.
fn exists(dir: &str, rev: &str) -> bool {
    git(dir, &["cat-file", "-e", &format!("{rev}^{{commit}}")]).is_ok()
}

fn is_ancestor(dir: &str, rev: &str, of: &str) -> bool {
    git(dir, &["merge-base", "--is-ancestor", rev, of]).is_ok()
}

/// The most recent of `revs`. Review commits are made one after another on one
/// line of development, so the newest of them is the tip of the chain.
/// `--no-walk=sorted` orders them without reading their history.
fn newest(dir: &str, revs: &[String]) -> Option<String> {
    if revs.is_empty() {
        return None;
    }
    let mut args = vec!["rev-list", "--no-walk=sorted"];
    args.extend(revs.iter().map(String::as_str));
    let out = git(dir, &args).ok()?;
    out.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Every remote this repository has, in git's order.
pub fn remotes(dir: &str) -> Vec<String> {
    git(dir, &["remote"])
        .map(|s| {
            s.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The configured remote whose GitHub repository is exactly `owner/name`.
/// Branch names are not enough for fork PRs: both the fix branch push and the
/// stacked PR have to stay in the repository that owns the reviewed head.
pub fn remote_for_github_repo(dir: &str, repository: &str) -> Option<String> {
    remotes(dir).into_iter().find(|remote| {
        let url = git(dir, &["remote", "get-url", remote]).ok();
        url.as_deref()
            .and_then(github_repo_from_url)
            .is_some_and(|slug| slug.eq_ignore_ascii_case(repository.trim()))
    })
}

fn github_repo_from_url(url: &str) -> Option<String> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    let start = lower.find("github.com")? + "github.com".len();
    let rest = url[start..]
        .trim_start_matches([':', '/'])
        .trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{name}"))
}

/// The remote a push should go to: whatever the branch is configured to push
/// to, else `origin`, else the only remote there is. `None` for a repository
/// with no remotes, and for one with several and no `origin` — choosing
/// between those is the reviewer's call, not ours.
pub fn default_remote(dir: &str, branch: Option<&str>) -> Option<String> {
    if let Some(b) = branch {
        if let Ok(r) = git(dir, &["config", "--get", &format!("branch.{b}.remote")]) {
            let r = r.trim().to_string();
            if !r.is_empty() {
                return Some(r);
            }
        }
    }
    let all = remotes(dir);
    if all.iter().any(|r| r == "origin") {
        return Some("origin".to_string());
    }
    match all.len() {
        1 => all.into_iter().next(),
        _ => None,
    }
}

/// The remote-tracking ref a branch is set to track, e.g. `origin/feature`.
fn upstream_ref(dir: &str, branch: &str) -> Option<String> {
    let spec = format!("{branch}@{{upstream}}");
    let out = git(
        dir,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", &spec],
    )
    .ok()?;
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_string())
}

/// Whether a fully-qualified ref exists here.
fn has_ref(dir: &str, full: &str) -> bool {
    git(dir, &["rev-parse", "--verify", "--quiet", full]).is_ok()
}

/// The commits in `to` that `from` does not have, newest first.
///
/// A `from` that names no commit — nothing, the empty tree, one of the
/// pseudo-bases an uncommitted review is scoped by — means the whole history
/// of `to`, which is the true answer to "what has the remote not got" for a
/// branch that has never been pushed. An unreadable range is no commits
/// rather than an error: this feeds a count and a safety check, and both are
/// better off cautious than absent.
pub fn commits_between(dir: &str, from: &str, to: &str) -> Vec<String> {
    let from = from.trim();
    let range = if from.is_empty() || from == EMPTY_TREE || from.starts_with(':') {
        to.to_string()
    } else {
        format!("{from}..{to}")
    };
    git(dir, &["rev-list", &range])
        .map(|s| {
            s.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Commits at HEAD that no branch and no remote-tracking ref can reach —
/// exactly what moving this checkout somewhere else would strand.
///
/// Empty for an attached HEAD, whose own branch always contains it, and empty
/// for a detached HEAD sitting on commits the remote already has. It is only
/// the third case that matters: a detached review that committed its fixes,
/// where nothing but the reflog knows where they went.
pub fn unreferenced_head(dir: &str) -> Vec<String> {
    git(
        dir,
        &["rev-list", "HEAD", "--not", "--branches", "--remotes"],
    )
    .map(|s| {
        s.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// The name of some branch that contains `rev`, with any remote prefix
/// stripped — `refs/remotes/origin/codex/cd` and `refs/heads/codex/cd` both
/// answer `codex/cd`. Used to work out what a stranded commit was built on
/// top of, so it can be saved under a name that says what it belongs to.
///
/// Full refnames rather than `%(refname:short)`, because only the full form
/// says where the remote name ends and a branch name with slashes in it
/// begins.
pub fn branch_containing(dir: &str, rev: &str) -> Option<String> {
    let out = git(
        dir,
        &[
            "for-each-ref",
            "--contains",
            rev,
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
    )
    .ok()?;
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.ends_with("/HEAD"))
        .find_map(|full| {
            if let Some(b) = full.strip_prefix("refs/heads/") {
                return Some(b.to_string());
            }
            // refs/remotes/<remote>/<branch...> — one segment of remote, then
            // the branch, slashes and all.
            full.strip_prefix("refs/remotes/")
                .and_then(|rest| rest.split_once('/'))
                .map(|(_, branch)| branch.to_string())
        })
}

/// Push `source` to `branch` on `remote`.
///
/// An explicit `<source>:refs/heads/<branch>` rather than a bare branch name,
/// so a detached review — the shape a branch already checked out elsewhere is
/// reviewed in — can publish the commits it made just the same, so commits
/// that are not on this checkout at all can still be sent, and so the
/// destination is never resolved from local configuration the caller did not
/// ask about.
pub fn push_branch(
    dir: &str,
    remote: &str,
    branch: &str,
    set_upstream: bool,
    source: &str,
) -> Result<String, String> {
    let refspec = format!("{source}:refs/heads/{branch}");
    let mut args = vec!["push"];
    if set_upstream {
        args.push("--set-upstream");
    }
    args.push(remote);
    args.push(&refspec);
    git(dir, &args)
}

/// Point a new branch at `at` without checking it out. Fails when the name is
/// taken, which is what should happen — silently reusing a branch would
/// publish whatever was already on it.
pub fn create_branch(dir: &str, name: &str, at: &str) -> Result<(), String> {
    git(dir, &["branch", name, at]).map(|_| ())
}

pub fn branch_exists(dir: &str, name: &str) -> bool {
    has_ref(dir, &format!("refs/heads/{name}"))
}

/// Move an existing branch to `target` without checking it out. Only ever
/// used to put a branch back where the remote has it, and only once the
/// commits being moved off it are safely pushed somewhere else.
pub fn move_branch(dir: &str, name: &str, target: &str) -> Result<(), String> {
    git(dir, &["branch", "--force", name, target]).map(|_| ())
}

/// A file that deletes itself, for handing prose to a process that wants a
/// path rather than an argument.
struct ScratchFile(std::path::PathBuf);

impl ScratchFile {
    fn new(tag: &str, contents: &str) -> Result<ScratchFile, String> {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{tag}-{}-{n}", std::process::id()));
        std::fs::write(&path, contents).map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(ScratchFile(path))
    }

    fn path(&self) -> String {
        self.0.to_string_lossy().to_string()
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Open a pull request from `head` into `base`, returning the URL `gh`
/// prints. `--head` is given explicitly so this does not depend on what the
/// worktree happens to have checked out.
///
/// The body goes through `--body-file` rather than `--body`: it is prose the
/// reviewer typed, so it has newlines and quotes in it, and passing that as a
/// command-line argument is a different quoting question on every platform —
/// one Windows answers by refusing outright when `gh` resolves to a batch
/// shim. A file also has no length limit to bump into.
pub fn pr_create(
    dir: &str,
    gh: &str,
    repository: Option<&str>,
    base: &str,
    head: &str,
    title: &str,
    body: &str,
) -> Result<String, String> {
    let body_file = ScratchFile::new("cra-pr-body", body)?;
    let body_path = body_file.path();
    let mut args = vec!["pr", "create"];
    if let Some(repository) = repository.filter(|r| !r.trim().is_empty()) {
        args.extend(["--repo", repository]);
    }
    args.extend([
        "--base",
        base,
        "--head",
        head,
        "--title",
        title,
        "--body-file",
        &body_path,
    ]);
    let out = run(dir, gh, &args)?;
    // `gh` prints the URL on its own line, with progress chatter around it.
    Ok(out
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("http"))
        .unwrap_or_else(|| out.trim())
        .to_string())
}

// ---------------------------------------------------------------------------
// GitHub PRs via the `gh` CLI

#[derive(Clone, Deserialize)]
pub struct PrAuthor {
    #[serde(default)]
    pub login: String,
}

#[derive(Clone, Deserialize)]
pub struct PrRepository {
    #[serde(rename = "nameWithOwner", default)]
    pub name_with_owner: String,
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
    #[serde(rename = "headRepository", default)]
    pub head_repository: Option<PrRepository>,
    #[serde(rename = "headRepositoryOwner", default)]
    pub head_repository_owner: Option<PrAuthor>,
}

impl PrInfo {
    /// Full repository identity for the PR head, including forks. Older `gh`
    /// output may omit `headRepository`; its owner plus this clone's repo name
    /// is an accurate fallback for GitHub forks.
    pub fn head_repo(&self, dir: &str) -> Option<String> {
        self.head_repository
            .as_ref()
            .map(|r| r.name_with_owner.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let owner = self.head_repository_owner.as_ref()?.login.trim();
                (!owner.is_empty()).then(|| format!("{owner}/{}", repo_name(dir)))
            })
    }
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
            "number,title,headRefName,baseRefName,author,headRepository,headRepositoryOwner",
        ],
    )?;
    serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))
}

/// Put a PR's head into this working tree. When the head branch is already
/// checked out in another worktree git refuses to switch, so fall back to a
/// detached checkout of the same commits rather than making the reviewer go
/// tidy up an unrelated worktree first. `Ok(Some(path))` names the worktree
/// that forced the fallback, so the UI can say why HEAD came back detached.
pub fn pr_checkout(dir: &str, gh: &str, number: u64) -> Result<Option<String>, String> {
    let n = number.to_string();
    let Err(e) = run(dir, gh, &["pr", "checkout", &n, "--force"]) else {
        return Ok(None);
    };
    let Some(other) = worktree_in_error(&e) else {
        return Err(e);
    };
    run(dir, gh, &["pr", "checkout", &n, "--detach", "--force"])
        .map_err(|d| format!("{e}\n\nchecking it out detached instead also failed: {d}"))?;
    Ok(Some(other))
}

/// The worktree path out of git's "is already used by worktree at '<path>'"
/// refusal — the one checkout failure a detached HEAD can still get past.
fn worktree_in_error(err: &str) -> Option<String> {
    let rest = err.split("is already used by worktree at").nth(1)?.trim();
    let rest = rest.strip_prefix('\'').unwrap_or(rest);
    Some(rest.split('\'').next().unwrap_or(rest).trim().to_string())
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
        assert!(
            s.contains(std::path::MAIN_SEPARATOR),
            "expected a full path, got {s}"
        );
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
    fn a_fork_pr_keeps_its_full_head_repository_identity() {
        let pr: PrInfo = serde_json::from_str(
            r#"{
                "number": 12,
                "title": "fork change",
                "headRefName": "feature",
                "baseRefName": "main",
                "headRepository": {"nameWithOwner": "contributor/widgets"},
                "headRepositoryOwner": {"login": "contributor"}
            }"#,
        )
        .expect("PR JSON");
        assert_eq!(
            pr.head_repo("C:/some/widgets").as_deref(),
            Some("contributor/widgets")
        );
    }

    #[test]
    fn a_github_repository_identity_selects_its_remote_not_origin_by_name() {
        let repo = TempRepo::new("fork-remote");
        repo.write("a.txt", "hi\n");
        repo.commit("first");
        repo.git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/base/widgets.git",
        ]);
        repo.git(&[
            "remote",
            "add",
            "contributor",
            "git@github.com:contributor/widgets.git",
        ]);

        assert_eq!(
            remote_for_github_repo(&repo.path(), "contributor/widgets").as_deref(),
            Some("contributor")
        );
        assert_eq!(
            remote_for_github_repo(&repo.path(), "base/widgets").as_deref(),
            Some("origin")
        );
    }

    #[test]
    fn dirtiness_follows_the_working_tree() {
        let repo = TempRepo::new("dirty");
        repo.write("a.txt", "hi\n");
        repo.commit("first");
        assert!(
            !is_dirty(&repo.path()),
            "a clean checkout must not read as dirty"
        );

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
        assert_eq!(
            names,
            vec!["feature", "main"],
            "most recently committed first"
        );
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
    fn a_branch_another_worktree_holds_is_still_reviewable_detached() {
        let repo = TempRepo::new("worktree");
        repo.write("a.txt", "hi\n");
        repo.commit("first");
        repo.git(&["branch", "feature"]);
        // A worktree's own branch never blocks it: `except` excuses itself.
        assert!(worktree_for_branch(&repo.path(), "main", &repo.path()).is_none());
        // Anyone else asking sees that the repository is standing on it.
        assert!(worktree_for_branch(&repo.path(), "main", "C:/nowhere").is_some());

        let elsewhere = TempDir::new("wt-linked");
        let other = elsewhere.path().join("held").to_string_lossy().to_string();
        repo.git(&["worktree", "add", &other, "feature"]);

        // Claimed elsewhere, so git refuses to switch to it here...
        assert!(worktree_for_branch(&repo.path(), "feature", &repo.path()).is_some());
        assert!(checkout(&repo.path(), "feature").is_err());

        // ...but its commits are reachable all the same, which is all a
        // review needs: the diff only ever reads base...HEAD.
        checkout_detached(&repo.path(), "feature").unwrap();
        assert_eq!(current_branch(&repo.path()).unwrap(), "HEAD");
        assert_eq!(
            head_sha(&repo.path()).unwrap(),
            repo.git(&["rev-parse", "feature"]).trim()
        );
    }

    #[test]
    fn worktree_refusal_names_the_worktree_that_caused_it() {
        let err = concat!(
            "gh pr checkout 3 failed: fatal: 'codex/cd' is already used by ",
            "worktree at 'C:/Users/eric/.codex/worktrees/7e55/cra'",
            "\n",
            "failed to run git: exit status 128"
        );
        assert_eq!(
            worktree_in_error(err).as_deref(),
            Some("C:/Users/eric/.codex/worktrees/7e55/cra")
        );
        // Every other failure is left alone — detaching would not help.
        assert!(worktree_in_error("fatal: pathspec 'nope' did not match").is_none());
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
        assert!(
            branch_diff.contains("Increment the counter"),
            "{branch_diff}"
        );

        // Against the empty tree: the whole history, so a brand-new repo has
        // something to review.
        let all = review_diff(&repo.path(), EMPTY_TREE, 12).unwrap();
        assert!(all.contains("src/lib.rs"), "{all}");

        // Empty base means the uncommitted working tree, and a clean tree
        // therefore has nothing to show.
        assert!(review_diff(&repo.path(), "", 12).unwrap().trim().is_empty());
        repo.write("src/lib.rs", LIB_RS.replace("one", "1").as_str());
        assert!(review_diff(&repo.path(), "", 12)
            .unwrap()
            .contains("counter"));
    }

    #[test]
    fn a_pr_is_diffed_against_the_server_base_not_a_stale_local_branch() {
        // An "upstream" repository, and a clone of it whose local `main` is
        // left behind — exactly what a reviewer who has not pulled in a while
        // is holding when they open somebody's pull request.
        let upstream = TempRepo::new("pr-base-up");
        upstream.write(
            "a.txt", "one
",
        );
        upstream.commit("first");
        let clone = TempRepo::new("pr-base-clone");
        clone.git(&["remote", "add", "origin", &upstream.path()]);
        clone.git(&["fetch", "--quiet", "origin"]);
        clone.git(&["reset", "--hard", "origin/main"]);

        // Upstream moves on, and the pull request branches from where it now is.
        upstream.write(
            "merged-since.txt",
            "landed on main
",
        );
        upstream.commit("second");
        upstream.git(&["checkout", "-b", "feature"]);
        upstream.write(
            "the-pr.txt",
            "the change under review
",
        );
        upstream.commit("the pull request");

        // The head is fetched, as `gh pr checkout` would; nothing updates the
        // clone's own `main`, which still points at the first commit.
        clone.git(&["fetch", "--quiet", "origin", "feature"]);
        clone.git(&["checkout", "--quiet", "--detach", "FETCH_HEAD"]);
        assert_eq!(clone.git(&["rev-list", "--count", "main"]).trim(), "1");

        // Diffing the stale local name drags in whatever landed on main since.
        let stale = review_diff(&clone.path(), "main", 3).unwrap();
        assert!(
            stale.contains("merged-since.txt"),
            "expected the stale base to leak: {stale}"
        );

        // The resolved base is the remote-tracking ref, and it shows the pull
        // request's own change and nothing else.
        let base = pr_base_ref(&clone.path(), "main");
        assert_eq!(base, "origin/main");
        let diff = review_diff(&clone.path(), &base, 3).unwrap();
        assert!(diff.contains("the-pr.txt"), "{diff}");
        assert!(
            !diff.contains("merged-since.txt"),
            "the base's own commits leaked in: {diff}"
        );
    }

    #[test]
    fn a_base_with_no_remote_tracking_ref_keeps_its_plain_name() {
        let repo = TempRepo::new("pr-base-local");
        repo.write(
            "a.txt", "one
",
        );
        repo.commit("first");
        assert_eq!(pr_base_ref(&repo.path(), "main"), "main");
    }

    #[test]
    fn base_label_names_each_kind_of_base() {
        assert_eq!(base_label(""), "HEAD");
        assert_eq!(base_label(EMPTY_TREE), "root");
        assert_eq!(base_label("main"), "main");
        // The sentinels must survive the label round-trip: the whole-branch review
        // re-runs the diff from the stored label alone.
        for base in ["", EMPTY_TREE, STAGED, UNTRACKED, "main"] {
            assert_eq!(base_from_label(base_label(base)), base);
        }
    }

    #[test]
    fn the_new_side_matches_what_each_diff_actually_ends_at() {
        assert!(
            new_side("") == NewSide::WorkTree,
            "bare diff shows uncommitted lines"
        );
        assert!(new_side("main") == NewSide::Head);
        assert!(new_side(EMPTY_TREE) == NewSide::Head);
        assert!(
            new_side(STAGED) == NewSide::Index,
            "staged lines live in the index only"
        );
        assert!(new_side(UNTRACKED) == NewSide::WorkTree);
    }

    #[test]
    fn staged_diff_shows_the_index_and_only_the_index() {
        let repo = TempRepo::new("staged");
        repo.write("a.txt", "one\n");
        repo.commit("first");

        // Nothing staged: nothing to review.
        assert!(review_diff(&repo.path(), STAGED, 12)
            .unwrap()
            .trim()
            .is_empty());

        // A staged edit shows; a later unstaged edit to the same file must not.
        repo.write("a.txt", "one\nstaged line\n");
        repo.git(&["add", "a.txt"]);
        repo.write("a.txt", "one\nstaged line\nunstaged line\n");
        let diff = review_diff(&repo.path(), STAGED, 12).unwrap();
        assert!(diff.contains("staged line"), "{diff}");
        assert!(
            !diff.contains("unstaged line"),
            "the unstaged edit leaked in: {diff}"
        );

        // And the extractor must read content from the index, not the dirty
        // worktree, or the diff's line numbers point into the wrong file.
        assert_eq!(
            file_at_index(&repo.path(), "a.txt").as_deref(),
            Some("one\nstaged line\n")
        );
        assert!(file_at_index(&repo.path(), "nope.txt").is_none());
    }

    #[test]
    fn untracked_base_appends_new_files_to_the_working_tree_diff() {
        let repo = TempRepo::new("untracked");
        repo.write("a.txt", "one\n");
        repo.write(".gitignore", "ignored.txt\n");
        repo.commit("first");

        repo.write("a.txt", "one\ntracked edit\n");
        repo.write("brand_new.rs", "fn shiny() {}\n");
        repo.write("ignored.txt", "never reviewed\n");

        // The plain working-tree diff cannot see the untracked file...
        let plain = review_diff(&repo.path(), "", 12).unwrap();
        assert!(!plain.contains("shiny"), "{plain}");

        // ...the untracked base sees both, but still honours .gitignore.
        let diff = review_diff(&repo.path(), UNTRACKED, 12).unwrap();
        assert!(diff.contains("tracked edit"), "{diff}");
        assert!(diff.contains("fn shiny() {}"), "{diff}");
        assert!(!diff.contains("never reviewed"), "{diff}");

        // The appended hunks must parse like any other new-file diff.
        let files = crate::diffparse::parse(&diff);
        assert!(
            files.iter().any(|f| f.path == "brand_new.rs"),
            "untracked file lost in parsing: {:?}",
            files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        assert_eq!(
            untracked_files(&repo.path()).unwrap(),
            vec!["brand_new.rs".to_string()]
        );
    }

    #[test]
    fn file_at_head_reads_the_commit_not_the_working_tree() {
        let repo = TempRepo::new("athead");
        repo.write("src/lib.rs", "committed\n");
        repo.commit("base");
        repo.write("src/lib.rs", "dirty\n");
        assert_eq!(
            file_at_head(&repo.path(), "src/lib.rs").as_deref(),
            Some("committed\n")
        );
        assert!(file_at_head(&repo.path(), "src/nope.rs").is_none());
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
        assert!(
            !last.contains("b.txt"),
            "b.txt should still be uncommitted: {last}"
        );
    }

    #[test]
    fn staged_revision_and_commit_never_copy_the_unstaged_worktree() {
        let repo = TempRepo::new("staged-index");
        repo.write("a.txt", "base\n");
        repo.commit("base");

        repo.write("a.txt", "staged\n");
        repo.git(&["add", "--", "a.txt"]);
        repo.write("a.txt", "unstaged\n");

        write_index_file(&repo.path(), "a.txt", "reviewed staged\n").unwrap();
        assert_eq!(
            file_at_index(&repo.path(), "a.txt").as_deref(),
            Some("reviewed staged\n")
        );
        assert_eq!(
            repo.read("a.txt"),
            "unstaged\n",
            "index edit touched the worktree"
        );

        commit_index(&repo.path(), "review: staged snapshot").unwrap();
        assert_eq!(
            file_at_head(&repo.path(), "a.txt").as_deref(),
            Some("reviewed staged\n")
        );
        assert_eq!(
            repo.read("a.txt"),
            "unstaged\n",
            "commit staged the unstaged copy"
        );
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
        assert_eq!(
            extracted.len(),
            1,
            "one file should have a reviewable comment"
        );
        let (path, units) = &extracted[0];
        assert_eq!(path, "src/lib.rs");
        assert_eq!(units.len(), 1);

        let unit = &units[0];
        assert_eq!(unit.lang, "Rust");
        assert!(unit.has_added);
        let file = ReviewFile::new(path.clone(), vec![]);
        let replacement = unit.format_replacement("Counting retries, not requests.");
        let wrapped = crate::units::ReviewUnit::Comment(unit.clone());
        let delta = apply_edit(&repo.path(), &file, &wrapped, &replacement).unwrap();

        assert_eq!(delta, 0, "one line replaced by one line");
        let after = repo.read("src/lib.rs");
        assert!(
            after.contains("    // Counting retries, not requests."),
            "indent lost: {after}"
        );
        assert!(!after.contains("Increment the counter"), "{after}");
        assert!(
            after.contains("counter += 1;"),
            "the code itself must be untouched: {after}"
        );
        assert!(
            is_dirty(&repo.path()),
            "the edit should show up as a working-tree change"
        );
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
        let file = ReviewFile::new(path.clone(), vec![]);

        // Someone edits the file behind our back.
        repo.write("src/lib.rs", "fn main() {\n    counter += 1;\n}\n");
        let wrapped = crate::units::ReviewUnit::Comment(units[0].clone());
        let err = apply_edit(&repo.path(), &file, &wrapped, &[]).unwrap_err();
        assert!(
            err.contains("mismatch") || err.contains("out of bounds"),
            "should refuse the stale edit, said: {err}"
        );
    }
}
