//! Getting a finished review's fix commits out of the review worktree.
//!
//! A branch or PR review runs in a worktree of its own (see [`crate::worktree`])
//! and commits its accepted edits there. That is the right place to *make*
//! them and the wrong place to leave them: the worktree is reused by the next
//! review, and until then the fixes exist on one machine, in a directory the
//! author of the code has never heard of.
//!
//! There are two honest ways out, and which one is right is a question about
//! permissions and etiquette rather than about git:
//!
//! * **Push** — the fixes belong on the branch that was reviewed. Your own
//!   branch, or a PR you have write access to and agreed to fix directly.
//! * **Stack** — the fixes go on a branch of their own and are proposed back
//!   as a pull request *targeting the reviewed branch*, so its author reviews
//!   the reviewer. Someone else's PR, a protected branch, or simply changes
//!   big enough to deserve their own discussion.
//!
//! Both are all-or-nothing about the commits, never about the working tree:
//! uncommitted edits are not published by either route, and the reviewer is
//! told so before they choose.

use crate::gitio::{self, Delivery};

/// What to do with the commits a review made.
pub enum Route {
    /// Push them onto the branch that was reviewed.
    Push,
    /// Put them on a branch of their own and open a pull request for it.
    Stack(Stack),
}

/// A stacked pull request as the reviewer filled it in.
pub struct Stack {
    /// The branch to create for the fixes.
    pub branch: String,
    /// The branch the pull request targets — the reviewed branch when the
    /// remote has it, so the stack is a real stack.
    pub base: String,
    pub title: String,
    pub body: String,
    /// Put the reviewed branch back where the remote has it, once the fixes
    /// are safely on their own branch and pushed. Only ever offered when
    /// [`restore_blocker`] says nothing stands in the way.
    pub restore: bool,
}

/// Everything one publish needs, read on the UI thread so the work itself —
/// which is network-bound and slow — can happen on another.
pub struct Request {
    /// The review worktree the commits are in.
    pub dir: String,
    pub gh: String,
    pub remote: String,
    /// What the reviewed branch is called on the remote.
    pub ref_name: String,
    /// The delivery state read when the summary screen was opened.
    pub state: Delivery,
    /// Commit shas this review session made, from the database. What makes
    /// restoring the reviewed branch provably safe.
    pub session_commits: Vec<String>,
    pub route: Route,
}

/// What a finished publish is worth telling the reviewer.
#[derive(Debug)]
pub struct Outcome {
    pub headline: String,
    /// The pull request `gh` opened, when the route made one.
    pub url: Option<String>,
    /// Everything else that happened, one line each — a branch created, an
    /// upstream set, a branch put back.
    pub detail: Vec<String>,
}

/// Why the reviewed branch cannot be put back where the remote has it, or
/// `None` when it can.
///
/// Restoring rewinds a branch, so it is offered only when every commit it
/// would drop is one this review made and is already published somewhere
/// else. Anything less certain — a detached review that never moved the
/// branch, a commit the reviewer made by hand, an unpushed branch with no
/// remote position to return to — and the branch is left exactly as it is.
pub fn restore_blocker(
    state: &Delivery,
    ref_name: &str,
    session_commits: &[String],
) -> Option<String> {
    if state.branch.as_deref() != Some(ref_name) {
        return Some(match &state.branch {
            // Where a second attempt after a failed `gh pr create` lands: the
            // first one already moved the worktree onto the fixes branch and
            // put the reviewed branch back.
            Some(other) => format!("the worktree is on {other}, not {ref_name}"),
            None => format!("the review ran on a detached HEAD, so {ref_name} was never moved"),
        });
    }
    // Nowhere to go back *to*: a branch the remote has never seen has no
    // position that is not the one the review put it in.
    let Some(upstream) = state.upstream.as_deref() else {
        return Some(format!("{ref_name} has no remote branch to go back to"));
    };
    if state.dirty {
        return Some("the review worktree has uncommitted changes".to_string());
    }
    let outside = state
        .unpushed
        .iter()
        .filter(|s| !session_commits.iter().any(|c| c == *s))
        .count();
    if outside > 0 {
        return Some(format!(
            "{outside} of the {} commit(s) on {ref_name} were not made by this review",
            state.unpushed.len()
        ));
    }
    if state.unpushed.is_empty() {
        return Some(format!("{ref_name} is already at {upstream}"));
    }
    None
}

/// A branch name for the fixes, derived from what was reviewed. Kept short
/// and predictable so the reviewer can recognise it in a branch list a week
/// later, and sanitised because a PR head ref can contain characters
/// `git branch` will not take.
pub fn suggested_branch(ref_name: &str) -> String {
    let slug: String = ref_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '/' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches(['-', '/']).to_string();
    let slug = if slug.is_empty() {
        "review".to_string()
    } else {
        slug
    };
    format!("review/{slug}-fixes")
}

/// Do it. Every step that can fail says what state it failed in, because the
/// steps are not individually undoable and a reviewer left guessing whether
/// the branch got pushed has to go and look.
pub fn run(req: &Request) -> Result<Outcome, String> {
    if req.state.unpushed.is_empty() {
        return Err("nothing to publish — the remote already has every commit here".to_string());
    }
    match &req.route {
        Route::Push => push(req),
        Route::Stack(stack) => self::stack(req, stack),
    }
}

fn push(req: &Request) -> Result<Outcome, String> {
    if req.state.behind > 0 {
        return Err(format!(
            "{} is {} commit(s) ahead of this review — pushing would be a non-fast-forward. \
             Pull them in first, or open a stacked pull request instead.",
            req.state.upstream.as_deref().unwrap_or(&req.ref_name),
            req.state.behind
        ));
    }
    // Only a branch can be given an upstream, and only the branch that is
    // actually being pushed: a detached review publishes the same commits but
    // has no local branch to configure.
    // Only a branch that is actually checked out here can be given an
    // upstream, and only when the tip being pushed is that branch's own.
    let set_upstream = req.state.upstream.is_none()
        && req.state.branch.as_deref() == Some(&req.ref_name)
        && req.state.tip == "HEAD";
    gitio::push_branch(
        &req.dir,
        &req.remote,
        &req.ref_name,
        set_upstream,
        &req.state.tip,
    )?;

    let n = req.state.ahead();
    let mut detail = Vec::new();
    if set_upstream {
        detail.push(format!(
            "{} now tracks {}/{}",
            req.ref_name, req.remote, req.ref_name
        ));
    }
    if req.state.dirty {
        detail.push(
            "uncommitted edits are still in the review worktree — they were not pushed".to_string(),
        );
    }
    Ok(Outcome {
        headline: format!("pushed {n} commit(s) to {}/{}", req.remote, req.ref_name),
        url: None,
        detail,
    })
}

fn stack(req: &Request, stack: &Stack) -> Result<Outcome, String> {
    let name = stack.branch.trim();
    if name.is_empty() {
        return Err("name the branch the fixes should go on".to_string());
    }
    if name == req.ref_name {
        return Err(format!(
            "{name} is the branch being reviewed — a stacked pull request needs a branch of its own"
        ));
    }
    if stack.title.trim().is_empty() {
        return Err("give the pull request a title".to_string());
    }
    if stack.base.trim().is_empty() {
        return Err("name the branch the pull request should target".to_string());
    }
    if stack.base.trim() == name {
        return Err(format!("{name} cannot be stacked on itself"));
    }
    // The commits being stacked, which are not always the ones checked out.
    let head = match req.state.tip.as_str() {
        "HEAD" => gitio::head_sha(&req.dir)?,
        sha => sha.to_string(),
    };
    let mut detail = Vec::new();

    // Re-running after a failed `gh pr create` must not be blocked by the
    // branch the first attempt already made, so a branch that is already
    // exactly where this one would go is reused rather than refused.
    if gitio::branch_exists(&req.dir, name) {
        let at = gitio::run(&req.dir, "git", &["rev-parse", name])?
            .trim()
            .to_string();
        if at != head {
            return Err(format!(
                "{name} already exists and points somewhere else ({}) — pick another name",
                &at[..8.min(at.len())]
            ));
        }
        detail.push(format!(
            "reused the existing branch {name}, already at these commits"
        ));
    } else {
        gitio::create_branch(&req.dir, name, &head)?;
        detail.push(format!("created {name} at {}", &head[..8.min(head.len())]));
    }

    if let Err(e) = gitio::push_branch(&req.dir, &req.remote, name, false, &head) {
        // Leave nothing behind for a retry to trip over — but only the branch
        // this attempt created, never one it found.
        if detail.last().is_some_and(|d| d.starts_with("created")) {
            let _ = gitio::run(&req.dir, "git", &["branch", "-D", name]);
        }
        return Err(e);
    }
    detail.push(format!(
        "pushed {} commit(s) to {}/{name}",
        req.state.ahead(),
        req.remote
    ));

    if stack.restore {
        match restore_blocker(&req.state, &req.ref_name, &req.session_commits) {
            Some(why) => detail.push(format!("left {} as it is: {why}", req.ref_name)),
            None => {
                // `git branch --force` refuses to move a branch that is checked
                // out, so the worktree moves onto the fixes branch first — which
                // is where it should be sitting now anyway.
                let upstream = req.state.upstream.clone().unwrap_or_default();
                gitio::checkout(&req.dir, name)?;
                gitio::move_branch(&req.dir, &req.ref_name, &upstream)?;
                detail.push(format!("put {} back at {upstream}", req.ref_name));
            }
        }
    }

    let url = gitio::pr_create(
        &req.dir,
        &req.gh,
        &stack.base,
        name,
        stack.title.trim(),
        &stack.body,
    )
    .map_err(|e| {
        format!(
            "{name} is pushed, but opening the pull request failed:\n{e}\n\nRetry, or run: \
             gh pr create --base {} --head {name}",
            stack.base
        )
    })?;

    if req.state.dirty {
        detail.push(
            "uncommitted edits are still in the review worktree — they were not published"
                .to_string(),
        );
    }
    Ok(Outcome {
        headline: format!("opened a pull request from {name} into {}", stack.base),
        url: Some(url),
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{FakeCli, FakeCliSpec, TempDir, TempRepo};

    /// A repository with a bare "remote" beside it, a `feature` branch pushed
    /// to it, and `n` further commits on top standing in for review fixes.
    /// Returns the repo, the remote's path, and the fix shas newest first.
    fn with_remote(tag: &str, fixes: usize) -> (TempRepo, TempDir, Vec<String>) {
        let repo = TempRepo::new(tag);
        repo.write("a.txt", "one\n");
        repo.commit("base");

        let remote = TempDir::new(&format!("{tag}-remote"));
        let remote_path = remote.path().to_string_lossy().replace('\\', "/");
        crate::gitio::run(&repo.path(), "git", &["init", "--bare", &remote_path])
            .expect("bare remote");
        repo.git(&["remote", "add", "origin", &remote_path]);
        repo.git(&["checkout", "-b", "feature"]);
        repo.write("a.txt", "two\n");
        repo.commit("the branch's own work");
        repo.git(&["push", "--set-upstream", "origin", "feature"]);

        let mut shas = Vec::new();
        for i in 0..fixes {
            repo.write("a.txt", &format!("fix {i}\n"));
            repo.commit(&format!("review: fix {i}"));
            shas.push(repo.git(&["rev-parse", "HEAD"]).trim().to_string());
        }
        shas.reverse();
        (repo, remote, shas)
    }

    fn state(repo: &TempRepo) -> Delivery {
        gitio::delivery_state(&repo.path(), "feature", "main", &[])
    }

    #[test]
    fn the_delivery_state_sees_the_remote_the_branch_and_what_is_unpushed() {
        let (repo, _remote, shas) = with_remote("pub-state", 2);
        let d = state(&repo);
        assert_eq!(d.branch.as_deref(), Some("feature"));
        assert_eq!(d.remote.as_deref(), Some("origin"));
        assert_eq!(d.upstream.as_deref(), Some("origin/feature"));
        assert_eq!(d.unpushed, shas, "only the fixes are unpushed");
        assert_eq!(d.behind, 0);
        assert!(!d.dirty);
        assert!(d.can_push());
    }

    /// The state that reads "1 commit made · 0 commits to publish": a detached
    /// review's commit, rescued onto a branch of its own, with HEAD since
    /// moved back to where the remote is. Asking HEAD what there is to publish
    /// answers "nothing" over a summary that just said a commit was made.
    #[test]
    fn a_session_commit_that_is_not_on_this_checkout_is_still_what_gets_published() {
        let (repo, remote, shas) = with_remote("pub-offhead", 1);
        let fix = shas[0].clone();
        // The shape `worktree::park_stranded` leaves: the commit on a branch of
        // its own, and the checkout detached back at the remote's position.
        repo.git(&["branch", "review/feature-fixes", &fix]);
        repo.git(&["checkout", "--detach", "origin/feature"]);
        // The rescued branch is the *only* thing holding it, as it is for a PR
        // head that was never a local branch in the first place.
        repo.git(&["branch", "--force", "feature", "origin/feature"]);
        assert_ne!(repo.git(&["rev-parse", "HEAD"]).trim(), fix);

        // Without the session's commits, HEAD has nothing to say.
        let blind = gitio::delivery_state(&repo.path(), "feature", "main", &[]);
        assert_eq!(blind.ahead(), 0, "this is the bug, seen from HEAD alone");

        let d = gitio::delivery_state(&repo.path(), "feature", "main", &shas);
        assert_eq!(d.tip, fix, "the review's own commit is the tip to publish");
        assert_eq!(
            d.tip_branch.as_deref(),
            Some("review/feature-fixes"),
            "and it says where"
        );
        assert_eq!(d.unpushed, vec![fix.clone()]);
        assert!(d.can_push());

        // And pushing really sends it, from a checkout that is not on it.
        let req = Request {
            dir: repo.path(),
            gh: "gh".into(),
            remote: "origin".into(),
            ref_name: "feature".into(),
            state: d,
            session_commits: shas,
            route: Route::Push,
        };
        run(&req).expect("push");
        let on_remote = gitio::run(
            &remote.path().to_string_lossy(),
            "git",
            &["rev-parse", "refs/heads/feature"],
        )
        .expect("remote head");
        assert_eq!(on_remote.trim(), fix);
    }

    /// The ordinary case must not pay for the one above: when the session's
    /// commits are on HEAD, the tip stays HEAD.
    #[test]
    fn commits_the_checkout_already_has_leave_the_tip_at_head() {
        let (repo, _remote, shas) = with_remote("pub-onhead", 2);
        let d = gitio::delivery_state(&repo.path(), "feature", "main", &shas);
        assert_eq!(d.tip, "HEAD");
        assert!(d.tip_branch.is_none());
        assert_eq!(d.unpushed, shas);
    }

    /// A commit the reflog has since dropped must not become the tip — asking
    /// git about a sha it no longer has is an error, not a fact.
    #[test]
    fn a_session_commit_that_no_longer_exists_is_ignored() {
        let (repo, _remote, shas) = with_remote("pub-gone", 1);
        let mut with_ghost = vec!["0".repeat(40)];
        with_ghost.extend(shas.clone());
        let d = gitio::delivery_state(&repo.path(), "feature", "main", &with_ghost);
        assert_eq!(d.tip, "HEAD", "the surviving commits are all on HEAD");
        assert_eq!(d.unpushed, shas);
    }

    #[test]
    fn a_repository_with_no_remote_offers_no_route_out() {
        let repo = TempRepo::new("pub-noremote");
        repo.write("a.txt", "one\n");
        repo.commit("base");
        let d = gitio::delivery_state(&repo.path(), "main", "", &[]);
        assert!(d.remote.is_none());
        assert!(d.upstream.is_none());
        assert!(!d.can_push(), "nothing to push to");
    }

    #[test]
    fn pushing_puts_the_fix_commits_on_the_remote_branch() {
        let (repo, remote, shas) = with_remote("pub-push", 2);
        let req = Request {
            dir: repo.path(),
            gh: "gh".into(),
            remote: "origin".into(),
            ref_name: "feature".into(),
            state: state(&repo),
            session_commits: shas.clone(),
            route: Route::Push,
        };
        let out = run(&req).expect("push");
        assert!(
            out.headline.contains("2 commit(s) to origin/feature"),
            "{}",
            out.headline
        );
        assert!(out.url.is_none());

        // The remote really has them, and the local branch is level again.
        let at = gitio::run(
            &remote.path().to_string_lossy(),
            "git",
            &["rev-parse", "refs/heads/feature"],
        )
        .expect("remote head");
        assert_eq!(at.trim(), shas[0]);
        assert!(state(&repo).unpushed.is_empty());
    }

    /// The one failure a push has that the reviewer can do something about:
    /// somebody else moved the branch. Saying so — and naming the other route
    /// — beats handing back git's own wall of text.
    #[test]
    fn a_branch_that_moved_under_the_review_is_refused_with_the_alternative() {
        let (repo, _remote, shas) = with_remote("pub-behind", 1);
        // Somebody else's commit, landed on the remote branch meanwhile.
        let mut d = state(&repo);
        d.behind = 1;
        let req = Request {
            dir: repo.path(),
            gh: "gh".into(),
            remote: "origin".into(),
            ref_name: "feature".into(),
            state: d,
            session_commits: shas,
            route: Route::Push,
        };
        let err = run(&req).expect_err("must refuse");
        assert!(err.contains("non-fast-forward"), "{err}");
        assert!(err.contains("stacked"), "the way out is named: {err}");
    }

    #[test]
    fn a_stacked_pr_pushes_a_branch_of_its_own_and_asks_gh_to_open_it() {
        let (repo, remote, shas) = with_remote("pub-stack", 2);
        let bin = TempDir::new("pub-stack-gh");
        let gh = FakeCli::new(
            &bin,
            "gh",
            FakeCliSpec {
                reply: "https://github.test/o/r/pull/7\n",
                ..Default::default()
            },
        );
        let req = Request {
            dir: repo.path(),
            gh: gh.command(),
            remote: "origin".into(),
            ref_name: "feature".into(),
            state: state(&repo),
            session_commits: shas.clone(),
            route: Route::Stack(Stack {
                branch: "review/feature-fixes".into(),
                base: "feature".into(),
                title: "Review fixes".into(),
                body: "two decisions".into(),
                restore: false,
            }),
        };
        let out = run(&req).expect("stack");
        assert_eq!(out.url.as_deref(), Some("https://github.test/o/r/pull/7"));

        // The fixes are on the remote under their own name, and the reviewed
        // branch on the remote has not moved.
        let remote_dir = remote.path().to_string_lossy().to_string();
        let stacked = gitio::run(
            &remote_dir,
            "git",
            &["rev-parse", "refs/heads/review/feature-fixes"],
        )
        .expect("stacked branch on the remote");
        assert_eq!(stacked.trim(), shas[0]);
        let reviewed = gitio::run(&remote_dir, "git", &["rev-parse", "refs/heads/feature"])
            .expect("reviewed branch");
        assert_ne!(
            reviewed.trim(),
            shas[0],
            "the reviewed branch was not pushed"
        );

        let argv = gh.argv_seen();
        assert!(argv.contains("pr create"), "{argv}");
        assert!(argv.contains("--base feature"), "{argv}");
        assert!(argv.contains("--head review/feature-fixes"), "{argv}");
    }

    /// The body is prose — newlines, backticks, whatever the reviewer typed —
    /// and handing that to a process as a command-line argument is a quoting
    /// question Windows answers by refusing to launch at all. It goes through
    /// a file, and the file has to still be readable when `gh` runs.
    #[test]
    fn a_multi_line_pull_request_body_reaches_gh_intact() {
        let (repo, _remote, shas) = with_remote("pub-body", 1);
        let bin = TempDir::new("pub-body-gh");
        let gh = FakeCli::new(
            &bin,
            "gh",
            FakeCliSpec {
                reply: "https://github.test/o/r/pull/3\n",
                ..Default::default()
            },
        );
        let body = "Fixes made while reviewing `feature`.\n\n- 2 unit(s) decided\n";
        let req = Request {
            dir: repo.path(),
            gh: gh.command(),
            remote: "origin".into(),
            ref_name: "feature".into(),
            state: state(&repo),
            session_commits: shas,
            route: Route::Stack(Stack {
                branch: "review/feature-fixes".into(),
                base: "feature".into(),
                title: "Review fixes".into(),
                body: body.to_string(),
                restore: false,
            }),
        };
        run(&req).expect("stack");
        let argv = gh.argv_seen();
        assert!(
            argv.contains("--body-file"),
            "the body is passed by path: {argv}"
        );
        assert!(
            !argv.contains("unit(s) decided"),
            "and not on the command line: {argv}"
        );
    }

    /// Restoring is the difference between a stack and a mess: without it the
    /// fixes sit on the reviewed branch *as well*, and the next push of that
    /// branch quietly swallows the pull request that was opened for them.
    #[test]
    fn restoring_rewinds_the_reviewed_branch_once_the_fixes_are_pushed() {
        let (repo, _remote, shas) = with_remote("pub-restore", 2);
        let before = repo
            .git(&["rev-parse", "origin/feature"])
            .trim()
            .to_string();
        let bin = TempDir::new("pub-restore-gh");
        let gh = FakeCli::new(
            &bin,
            "gh",
            FakeCliSpec {
                reply: "https://github.test/o/r/pull/8\n",
                ..Default::default()
            },
        );
        let req = Request {
            dir: repo.path(),
            gh: gh.command(),
            remote: "origin".into(),
            ref_name: "feature".into(),
            state: state(&repo),
            session_commits: shas.clone(),
            route: Route::Stack(Stack {
                branch: "review/feature-fixes".into(),
                base: "feature".into(),
                title: "Review fixes".into(),
                body: String::new(),
                restore: true,
            }),
        };
        let out = run(&req).expect("stack");
        assert!(
            out.detail.iter().any(|d| d.contains("put feature back")),
            "{:?}",
            out.detail
        );
        assert_eq!(
            repo.git(&["rev-parse", "feature"]).trim(),
            before,
            "feature is back"
        );
        // Nothing was lost: the fixes are on the branch the worktree now holds.
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).trim(), shas[0]);
        assert_eq!(
            gitio::current_branch(&repo.path()).unwrap(),
            "review/feature-fixes"
        );
    }

    #[test]
    fn a_commit_the_review_did_not_make_stops_the_branch_being_rewound() {
        let (repo, _remote, shas) = with_remote("pub-handmade", 2);
        let d = state(&repo);
        // The reviewer's own commit is in the range, so the range is not ours
        // to drop.
        let mine = &shas[1..];
        let why = restore_blocker(&d, "feature", mine).expect("must refuse");
        assert!(why.contains("not made by this review"), "{why}");
        assert!(
            restore_blocker(&d, "feature", &shas).is_none(),
            "all ours is fine"
        );
    }

    /// A branch the remote has never seen has no position to be put back to,
    /// and the check has to say so *before* the stacked branch is pushed —
    /// discovering it afterwards would fail a publish that had already
    /// happened.
    #[test]
    fn a_branch_with_no_remote_position_is_never_rewound() {
        let (repo, _remote, shas) = with_remote("pub-noupstream", 1);
        let mut d = state(&repo);
        d.upstream = None;
        let why = restore_blocker(&d, "feature", &shas).expect("must refuse");
        assert!(why.contains("no remote branch to go back to"), "{why}");
    }

    #[test]
    fn a_detached_review_never_moved_the_branch_so_there_is_nothing_to_restore() {
        let (repo, _remote, shas) = with_remote("pub-detached", 1);
        let mut d = state(&repo);
        d.branch = None;
        let why = restore_blocker(&d, "feature", &shas).expect("must refuse");
        assert!(why.contains("detached"), "{why}");
    }

    #[test]
    fn a_pr_head_ref_becomes_a_branch_name_git_will_take() {
        assert_eq!(suggested_branch("feature"), "review/feature-fixes");
        assert_eq!(
            suggested_branch("user:their branch"),
            "review/user-their-branch-fixes"
        );
        assert_eq!(suggested_branch(""), "review/review-fixes");
    }

    #[test]
    fn a_stacked_branch_left_by_a_failed_attempt_is_reused_rather_than_refused() {
        let (repo, _remote, shas) = with_remote("pub-retry", 1);
        let bin = TempDir::new("pub-retry-gh");
        let gh = FakeCli::new(
            &bin,
            "gh",
            FakeCliSpec {
                reply: "https://github.test/o/r/pull/9\n",
                ..Default::default()
            },
        );
        repo.git(&["branch", "review/feature-fixes"]);
        let req = Request {
            dir: repo.path(),
            gh: gh.command(),
            remote: "origin".into(),
            ref_name: "feature".into(),
            state: state(&repo),
            session_commits: shas,
            route: Route::Stack(Stack {
                branch: "review/feature-fixes".into(),
                base: "feature".into(),
                title: "Review fixes".into(),
                body: String::new(),
                restore: false,
            }),
        };
        let out = run(&req).expect("stack");
        assert!(
            out.detail.iter().any(|d| d.contains("reused")),
            "{:?}",
            out.detail
        );
    }

    #[test]
    fn a_name_already_taken_by_something_else_is_refused_before_anything_is_pushed() {
        let (repo, _remote, shas) = with_remote("pub-taken", 1);
        repo.git(&["branch", "review/feature-fixes", "main"]);
        let req = Request {
            dir: repo.path(),
            gh: "gh".into(),
            remote: "origin".into(),
            ref_name: "feature".into(),
            state: state(&repo),
            session_commits: shas,
            route: Route::Stack(Stack {
                branch: "review/feature-fixes".into(),
                base: "feature".into(),
                title: "Review fixes".into(),
                body: String::new(),
                restore: false,
            }),
        };
        let err = run(&req).expect_err("must refuse");
        assert!(err.contains("points somewhere else"), "{err}");
    }
}
