//! The isolated checkout a branch or PR review runs in.
//!
//! Reviewing used to move the reviewer's own working tree — `git checkout` for
//! a branch, `gh pr checkout` for a PR. That refuses to run while the tree is
//! dirty, fails outright when the branch is already checked out somewhere
//! else, and leaves whoever was mid-task standing on a different commit than
//! they left. None of it is necessary: a ranged review reads `base...HEAD` and
//! the files at HEAD, so it can happen in a worktree of its own and give the
//! reviewer's checkout back exactly as it was found.
//!
//! One worktree per repository, reused. Re-pointing it at the next branch
//! costs a checkout; a directory per branch would cost a copy of the tree.

use std::path::{Path, PathBuf};

use crate::gitio;

/// A prepared worktree, and how it turned out.
#[derive(Debug)]
pub struct Ready {
    /// The directory the review runs in.
    pub path: String,
    /// The worktree that already held the target branch, when one did. Git
    /// hands a branch out once, so this one took the commits detached
    /// instead — which reviews the same code but commits nowhere useful.
    pub held_by: Option<String>,
    /// Commits the *last* review left on no branch, saved before this one
    /// re-pointed the worktree over them. See [`park_stranded`].
    pub parked: Option<Parked>,
}

/// Review commits rescued from a detached HEAD, and the branch they were put
/// on so they can still be found.
#[derive(Debug)]
pub struct Parked {
    pub branch: String,
    pub commits: usize,
}

impl Ready {
    /// What the reviewer is told about where their review went. Every case
    /// here is worth saying: a checkout that quietly happened somewhere else
    /// is exactly the surprise this module exists to avoid, and so is a branch
    /// appearing that the reviewer did not ask for.
    pub fn describe(&self, what: &str) -> String {
        let mut out = match &self.held_by {
            Some(other) => format!(
                "{what} is checked out in the worktree at {other}, so the review runs on a \
                 detached HEAD in {} — a commit made here lands on no branch.",
                self.path
            ),
            None => format!(
                "reviewing {what} in an isolated worktree at {} — your own checkout is untouched.",
                self.path
            ),
        };
        if let Some(p) = &self.parked {
            out.push_str(&format!(
                " The last review left {} commit(s) here on no branch; they were saved to {} \
                 before this checkout, and can be pushed or stacked from its summary screen.",
                p.commits, p.branch
            ));
        }
        out
    }
}

/// Where a repository's review worktree lives: beside the database, never
/// inside the repository itself — a nested worktree shows up as an untracked
/// directory, which makes the repository read as dirty from then on.
pub fn dir_for(repo: &str) -> PathBuf {
    let mut dir = match std::env::var("CRA_WORKTREES") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            let mut d = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
            d.push("code-review-assistant");
            d.push("worktrees");
            d
        }
    };
    dir.push(key(repo));
    dir
}

/// A stable directory name for a repository: its own name, so a human can
/// tell what is in there, plus a digest of the full path, so two checkouts of
/// the same project cannot land on each other.
fn key(repo: &str) -> String {
    let name: String = gitio::repo_name(repo)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{name}-{:016x}", digest(&gitio::path_key(repo)))
}

/// FNV-1a. Not a security boundary — just a short, stable spelling of a path.
fn digest(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// The review worktree for `repo`, with `branch` checked out in it.
pub fn for_branch(repo: &str, branch: &str) -> Result<Ready, String> {
    let dir = dir_for(repo);
    let path = dir.to_string_lossy().to_string();
    gitio::worktree_prune(repo);
    // This worktree still holding the branch from the last review is not a
    // conflict with itself — it is the thing being reused.
    let held_by = gitio::worktree_for_branch(repo, branch, &path);
    let mut parked = None;
    if is_worktree(&dir) {
        guard_clean(&path)?;
        // Before the checkout, not after: this is the last moment the commits
        // it is about to move off are still findable.
        parked = park_stranded(&path)?;
        match held_by {
            Some(_) => gitio::checkout_detached(&path, branch)?,
            None => gitio::checkout(&path, branch)?,
        }
    } else {
        clear(&dir)?;
        match held_by {
            Some(_) => gitio::worktree_add_detached(repo, &path, branch)?,
            None => gitio::worktree_add(repo, &path, branch)?,
        }
    }
    Ok(Ready {
        path,
        held_by,
        parked,
    })
}

/// The review worktree for `repo`, with pull request `number` checked out in
/// it. `gh` does the fetching, so this only has to hand it a directory.
pub fn for_pr(repo: &str, gh: &str, number: u64) -> Result<Ready, String> {
    let dir = dir_for(repo);
    let path = dir.to_string_lossy().to_string();
    gitio::worktree_prune(repo);
    let mut parked = None;
    if is_worktree(&dir) {
        guard_clean(&path)?;
        // `gh pr checkout` is a checkout like any other, and a PR review is
        // the case most likely to have been detached in the first place.
        parked = park_stranded(&path)?;
    } else {
        clear(&dir)?;
        // Detached at the repository's HEAD: a starting point that collides
        // with no branch, and `gh` is about to move it anyway.
        gitio::worktree_add_detached(repo, &path, "HEAD")?;
    }
    let held_by = gitio::pr_checkout(&path, gh, number)?;
    Ok(Ready {
        path,
        held_by,
        parked,
    })
}

/// A linked worktree carries a `.git` *file* pointing back at the repository.
/// Anything else at that path is not one, whatever else it may be.
fn is_worktree(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Make the path free for `git worktree add`, which insists on creating the
/// directory itself. Only ever aimed at our own directory under the app's
/// data dir, and only when what is there is not a worktree.
fn clear(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)
            .map_err(|e| format!("clear stale worktree {}: {e}", dir.display()))?;
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    Ok(())
}

/// Refuse to re-point a worktree that still holds uncommitted work: the
/// checkout would carry away fixes nobody has committed yet, and they were
/// not written anywhere the reviewer would think to look for them.
fn guard_clean(path: &str) -> Result<(), String> {
    if gitio::is_dirty(path) {
        return Err(format!(
            "the review worktree at {path} has uncommitted changes — commit or discard them \
             before reviewing something else"
        ));
    }
    Ok(())
}

/// Put a branch on commits this worktree holds that no ref would keep, before
/// the next checkout moves off them.
///
/// A branch review commits onto the branch, which keeps the commits. A
/// *detached* review — what a branch already checked out elsewhere gets — has
/// no branch to commit onto, so re-pointing the worktree leaves its fixes
/// reachable only through the reflog, where they quietly expire. Refusing
/// would deadlock the reviewer: the summary screen is where those commits get
/// published, and reaching it means selecting a ref, which means coming
/// through here. So they are saved rather than blocked on, under the name the
/// stacked-pull-request flow would have given them — which that flow then
/// recognises as already holding exactly these commits.
fn park_stranded(path: &str) -> Result<Option<Parked>, String> {
    let stranded = gitio::unreferenced_head(path);
    if stranded.is_empty() {
        return Ok(None);
    }
    let head = gitio::head_sha(path)?;
    let short = &head[..8.min(head.len())];
    // What the commits were built on tells us what they are fixes *for*; the
    // sha is the fallback for a first commit with nothing under it.
    let name = match stranded
        .last()
        .and_then(|oldest| gitio::branch_containing(path, &format!("{oldest}^")))
        .map(|b| crate::publish::suggested_branch(&b))
    {
        Some(n) if !gitio::branch_exists(path, &n) => n,
        // Taken already, by an earlier rescue or by the reviewer: never move
        // someone else's branch to make room.
        Some(n) => format!("{n}-{short}"),
        None => format!("review/stranded-{short}"),
    };
    gitio::create_branch(path, &name, &head)?;
    Ok(Some(Parked {
        branch: name,
        commits: stranded.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{FakeCli, FakeCliSpec, TempDir, TempRepo, WorktreeRoot};

    #[test]
    fn two_checkouts_of_one_project_get_their_own_worktrees() {
        assert_ne!(key("C:/work/widgets"), key("C:/other/widgets"));
        // The same path spelled two ways is still one repository.
        assert_eq!(key("C:/work/widgets"), key("C:/work/widgets/"));
        assert!(key("C:/work/widgets").starts_with("widgets-"));
    }

    #[test]
    fn a_branch_review_lands_in_its_own_worktree_and_leaves_the_repo_alone() {
        let _root = WorktreeRoot::new("wt-root");
        let repo = TempRepo::new("wt-branch");
        repo.write("a.txt", "one\n");
        repo.commit("first");
        repo.git(&["checkout", "-b", "feature"]);
        repo.write("a.txt", "two\n");
        repo.commit("second");
        repo.git(&["checkout", "main"]);
        // Uncommitted work that a checkout here would have refused to disturb.
        repo.write("a.txt", "scratch\n");

        let ready = for_branch(&repo.path(), "feature").expect("worktree");
        assert!(ready.held_by.is_none(), "nothing else holds the branch");
        assert_eq!(gitio::current_branch(&ready.path).unwrap(), "feature");
        assert_eq!(
            std::fs::read_to_string(Path::new(&ready.path).join("a.txt")).unwrap(),
            "two\n"
        );

        // The reviewer's checkout is exactly where they left it.
        assert_eq!(gitio::current_branch(&repo.path()).unwrap(), "main");
        assert_eq!(repo.read("a.txt"), "scratch\n");

        // Asked again, the same directory is reused rather than piling up.
        let again = for_branch(&repo.path(), "feature").expect("reuse");
        assert_eq!(again.path, ready.path);
        assert!(
            again.held_by.is_none(),
            "its own hold on the branch is not a conflict"
        );
    }

    #[test]
    fn a_pr_review_gets_a_worktree_and_gh_is_run_inside_it() {
        let _root = WorktreeRoot::new("wt-root4");
        let repo = TempRepo::new("wt-pr");
        repo.write(
            "a.txt", "one
",
        );
        repo.commit("first");

        let bin = TempDir::new("wt-gh");
        let gh = FakeCli::new(
            &bin,
            "gh",
            FakeCliSpec {
                exit_code: 0,
                ..Default::default()
            },
        );
        let ready = for_pr(&repo.path(), &gh.command(), 3).expect("worktree");

        // `gh` was asked for the PR, and asked for it in the new worktree.
        assert!(ready.held_by.is_none(), "{ready:?}");
        assert!(
            gh.argv_seen().contains("pr checkout 3 --force"),
            "{}",
            gh.argv_seen()
        );
        assert!(
            gitio::same_path(&gh.cwd_seen(), &ready.path),
            "{} is not {}",
            gh.cwd_seen(),
            ready.path
        );

        // Detached to start with, so no branch of the reviewer's was claimed,
        // and their checkout is untouched either way.
        assert_eq!(gitio::current_branch(&ready.path).unwrap(), "HEAD");
        assert_eq!(gitio::current_branch(&repo.path()).unwrap(), "main");
    }

    /// The bug this exists for: a detached review commits its fixes onto no
    /// branch, and the next review's checkout used to leave them reachable
    /// only through the reflog, where they expire. Nothing said so, and
    /// nothing could get them back.
    #[test]
    fn a_detached_reviews_commits_are_saved_before_the_next_checkout_moves_off_them() {
        let _root = WorktreeRoot::new("wt-root5");
        let repo = TempRepo::new("wt-strand");
        repo.write("a.txt", "one\n");
        repo.commit("first");
        repo.git(&["branch", "codex/cd"]);
        repo.git(&["branch", "other"]);

        // `codex/cd` is held by the repository's own checkout only when it is
        // checked out there; take it detached the way a held branch is.
        repo.git(&["checkout", "codex/cd"]);
        let ready = for_branch(&repo.path(), "codex/cd").expect("worktree");
        assert!(
            ready.held_by.is_some(),
            "the repo holds the branch, so this is detached"
        );
        assert_eq!(gitio::current_branch(&ready.path).unwrap(), "HEAD");

        // A fix commit, made where nothing will keep it.
        std::fs::write(Path::new(&ready.path).join("a.txt"), "fixed\n").unwrap();
        gitio::stage_and_commit(&ready.path, "a.txt", "review(code): fix it").expect("commit");
        let fix = gitio::head_sha(&ready.path).expect("sha");
        assert!(
            gitio::unreferenced_head(&ready.path).contains(&fix),
            "nothing holds it yet"
        );

        // Reviewing something else re-points the worktree over those commits.
        let next = for_branch(&repo.path(), "other").expect("worktree");
        let parked = next
            .parked
            .as_ref()
            .expect("the commits were saved, not dropped");
        assert_eq!(parked.commits, 1);
        // Named for what they are fixes *for*, which is the name the stacked
        // pull request would give them too — so stacking them later finds the
        // branch already holding exactly these commits rather than colliding.
        assert_eq!(parked.branch, crate::publish::suggested_branch("codex/cd"));
        assert_eq!(parked.branch, "review/codex/cd-fixes");
        assert_eq!(
            repo.git(&["rev-parse", &parked.branch]).trim(),
            fix,
            "the branch really points at the fix"
        );
        assert!(
            next.describe("other").contains(&parked.branch),
            "and the reviewer is told: {}",
            next.describe("other")
        );
    }

    /// A branch review commits onto the branch, which keeps them. Saving a
    /// second copy of those would litter the branch list on every review.
    #[test]
    fn commits_a_branch_already_holds_are_not_parked_again() {
        let _root = WorktreeRoot::new("wt-root6");
        let repo = TempRepo::new("wt-nostrand");
        repo.write("a.txt", "one\n");
        repo.commit("first");
        repo.git(&["branch", "feature"]);
        repo.git(&["branch", "other"]);

        let ready = for_branch(&repo.path(), "feature").expect("worktree");
        assert_eq!(gitio::current_branch(&ready.path).unwrap(), "feature");
        std::fs::write(Path::new(&ready.path).join("a.txt"), "fixed\n").unwrap();
        gitio::stage_and_commit(&ready.path, "a.txt", "review(code): fix it").expect("commit");
        assert!(
            gitio::unreferenced_head(&ready.path).is_empty(),
            "the branch holds them"
        );

        let next = for_branch(&repo.path(), "other").expect("worktree");
        assert!(
            next.parked.is_none(),
            "nothing to save — feature still points at the fix"
        );
        assert_eq!(
            repo.git(&["rev-parse", "feature"]).trim().len(),
            40,
            "feature is still a branch"
        );
    }

    #[test]
    fn a_branch_held_elsewhere_is_taken_detached() {
        let _root = WorktreeRoot::new("wt-root2");
        let repo = TempRepo::new("wt-held");
        repo.write("a.txt", "one\n");
        repo.commit("first");
        // The reviewer's own checkout counts: git will not hand `main` out twice.
        let ready = for_branch(&repo.path(), "main").expect("worktree");
        let held = ready
            .held_by
            .clone()
            .expect("the repository itself holds main");
        assert!(gitio::same_path(&held, &repo.path()), "{held}");
        assert_eq!(gitio::current_branch(&ready.path).unwrap(), "HEAD");
        assert!(
            ready.describe("main").contains("detached"),
            "{}",
            ready.describe("main")
        );
    }

    #[test]
    fn uncommitted_review_fixes_are_not_checked_out_from_under_the_reviewer() {
        let _root = WorktreeRoot::new("wt-root3");
        let repo = TempRepo::new("wt-dirty");
        repo.write("a.txt", "one\n");
        repo.commit("first");
        repo.git(&["branch", "feature"]);
        repo.git(&["branch", "other"]);

        let ready = for_branch(&repo.path(), "feature").expect("worktree");
        let fix = Path::new(&ready.path).join("a.txt");
        std::fs::write(&fix, "half-finished fix\n").unwrap();

        let err = for_branch(&repo.path(), "other").expect_err("must refuse");
        assert!(err.contains("uncommitted changes"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&fix).unwrap(),
            "half-finished fix\n",
            "the fix is still there"
        );
    }
}
