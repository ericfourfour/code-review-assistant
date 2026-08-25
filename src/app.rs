//! Top-level egui application: state, screen routing, hotkeys, and asynchronous model calls.

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};

use crate::db::Db;
use crate::discover::{self, DiscoveredRepo};
use crate::findings::{self, Finding, WholeBranchReviewMsg};
use crate::gitio::{self, BranchInfo, PrInfo};
use crate::models::{self, Action, CandidateMsg, Evidence, Suggestion, Turn};
use crate::notes::Note;
use crate::procs::{Owner, ProcHandle, ProcTable, StopReceipt};
use crate::publish;
use crate::review::{self, Choice, RefKind, ReviewFile, ReviewPlan};
use crate::settings::{ModelConfig, Settings};
use crate::triage;
use crate::units::{self, ReviewUnit};
use crate::worktree;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    RepoPicker,
    RefPicker,
    FilePicker,
    Review,
    Summary,
    Followup,
    Eval,
    Settings,
}

impl Screen {
    pub fn label(self) -> &'static str {
        match self {
            Screen::RepoPicker => "repository picker",
            Screen::RefPicker => "branch picker",
            Screen::FilePicker => "file picker",
            Screen::Review => "review screen",
            Screen::Summary => "summary screen",
            Screen::Followup => "follow-up screen",
            Screen::Eval => "evaluation page",
            Screen::Settings => "settings",
        }
    }

    /// The CLI work this screen owns, if it drives any. Leaving a screen ends
    /// what it owns, so this mapping is what makes "navigating away stops the
    /// models" a rule rather than a habit each screen has to remember.
    pub fn owns(self) -> Option<Owner> {
        match self {
            Screen::Review => Some(Owner::Review),
            Screen::Summary => Some(Owner::Branch),
            Screen::Followup => Some(Owner::Fix),
            _ => None,
        }
    }
}

/// A call that was stopped before it finished, kept so the reviewer can pick
/// it up again rather than start over.
///
/// The session id is the whole question. A model whose id this app generates
/// (`{session}` in its command) has one from the moment it launches, so an
/// interrupted call can be resumed into the same conversation. A model that
/// only reports its id in its reply leaves none behind when it is killed
/// mid-answer — there is nothing to resume, and the card says so instead of
/// offering a button that would quietly start a new conversation.
#[derive(Clone)]
pub struct PausedCall {
    pub owner: Owner,
    pub model_index: usize,
    pub model: String,
    /// What the call was doing when it was stopped.
    pub what: String,
    /// The process that was killed. Kept so the claim is checkable: this is
    /// the number that was in Task Manager.
    pub pid: Option<u32>,
    pub session: Option<String>,
    pub ran_for: std::time::Duration,
    pub usage: models::Usage,
    /// The prompt the stopped call was answering, so resuming asks the same
    /// question again rather than a remembered summary of it.
    pub prompt: String,
    /// Why it stopped, in the words the reviewer will read.
    pub reason: String,
}

impl PausedCall {
    /// Whether the same conversation can be continued: there is a session id,
    /// and the CLI holding it knows how to resume one.
    pub fn resumable(&self, settings: &Settings) -> bool {
        self.session.is_some()
            && settings.models.get(self.model_index).is_some_and(|m| {
                let command = match self.owner {
                    Owner::Fix => &m.fix_resume_command,
                    Owner::Review | Owner::Branch => &m.resume_command,
                };
                !command.trim().is_empty()
            })
    }

    /// The one-line state a paused card shows. `identifying` is false on a
    /// blinded card, where the pid is withheld — see [`crate::ui::procs_panel::PausedView`].
    pub fn line(&self, identifying: bool) -> String {
        let ran = format!("paused after {}s", self.ran_for.as_secs());
        if !identifying {
            return ran;
        }
        let pid = match self.pid {
            Some(pid) => format!("pid {pid} terminated"),
            None => "no process was running".to_string(),
        };
        format!("{ran} · {pid}")
    }

    /// The session as a human reads it — short enough for a card, whole
    /// enough to match against what the CLI itself lists. On a blinded card
    /// the id itself is withheld and only the answer the reviewer needs — can
    /// this be picked up again — is given.
    pub fn session_line(&self, identifying: bool) -> String {
        match (&self.session, identifying) {
            (Some(id), true) => format!("session {id}"),
            (Some(_), false) => "the conversation is still open".to_string(),
            (None, _) => "no session id — the CLI reports one only when it finishes".to_string(),
        }
    }
}

/// What leaving a screen did, shown on the screen the reviewer lands on until
/// they dismiss it.
///
/// This exists because a stop the reviewer cannot see is indistinguishable
/// from a leak. The receipts are live handles, not a rendered string, so the
/// banner goes from "terminating…" to "terminated" as each process actually
/// confirms it — the confirmation is of the kill, not of the request.
pub struct NavNotice {
    pub left: Screen,
    pub receipts: Vec<StopReceipt>,
    /// Sessions that can be picked up again, of those stopped.
    pub resumable: usize,
}

impl NavNotice {
    /// Whether every process named in the notice has confirmed it is gone.
    pub fn all_confirmed(&self) -> bool {
        self.receipts.iter().all(|r| r.confirmed())
    }

    pub fn headline(&self) -> String {
        let n = self.receipts.len();
        let confirmed = self.receipts.iter().filter(|r| r.confirmed()).count();
        let verb = if self.all_confirmed() {
            "terminated"
        } else {
            "terminating"
        };
        format!(
            "left the {} — {confirmed}/{n} process(es) {verb}, {} session(s) paused",
            self.left.label(),
            self.resumable
        )
    }
}

/// What identifies the unit under review: where it is and what it says. The
/// text is part of it because a unit reloaded from disk is a different
/// question even at the same line, and its old answers do not apply to it.
pub type UnitKey = (String, u32, String);

/// The key for a unit, matching the identity a prefetch is claimed by.
pub fn unit_key(unit: &ReviewUnit) -> UnitKey {
    (
        unit.file().to_string(),
        unit.start_line(),
        unit.raw_lines().join("\n"),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RefTab {
    Branches,
    Prs,
}

pub struct RepoCtx {
    pub path: String,
    pub name: String,
    pub default_branch: String,
}

pub enum CandidateState {
    Disabled,
    /// Running; the handle is the live view updated by the worker thread.
    Pending(ProcHandle),
    /// Stopped before it answered, because the reviewer left the page. What
    /// is left of the call is here so the same session can be picked back up.
    Paused(PausedCall),
    Ready(Suggestion),
    Failed(String),
}

/// One model's early reply for a unit the review has not reached yet, held
/// verbatim so adopting it can do everything a live reply's arrival would.
pub struct PrefetchedReply {
    pub result: Result<Suggestion, String>,
    pub raw: String,
}

/// Model queries started for an upcoming unit while the current one is still
/// being decided. Deciding takes minutes and the models seconds, so the
/// verdicts are usually waiting by the time the review advances.
pub struct Prefetch {
    /// Sequence id its replies arrive under; adopted as `review_seq` when the
    /// review reaches the unit, so a model still running can reply normally.
    pub seq: u64,
    /// Session the queries were started under, so a reply is billed to it even
    /// if it arrives after the plan has moved on.
    pub session_id: i64,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    /// The unit text as prefetched. A unit reloaded from disk since no longer
    /// matches, and its prefetch is discarded rather than trusted.
    pub unit_text: String,
    pub prompt: String,
    /// Exact configuration used to start these calls. Settings may be edited
    /// while a process is running; replies keep their original identity.
    pub models: Vec<ModelConfig>,
    /// Pre-generated ids for models whose command carries `{session}`.
    pub sessions: Vec<Option<String>>,
    /// Model indexes that were actually queried, so completeness is judged
    /// against what was queried rather than the settings as they are now.
    pub spawned: Vec<usize>,
    /// Parallel to the settings models; `None` means the query is still running.
    pub replies: Vec<Option<PrefetchedReply>>,
    /// Parallel to the settings models: the live view of each spawned call,
    /// adopted along with the prefetch so the elapsed clock reads from when
    /// the call actually started rather than from when the review arrived.
    pub lives: Vec<Option<ProcHandle>>,
    /// Set once the unit has been pushed to the back of its file, so a
    /// unanimous keep is deferred at most once and the review terminates.
    pub deferred: bool,
}

impl Prefetch {
    pub fn is_for(&self, unit: &ReviewUnit) -> bool {
        self.file == unit.file()
            && self.start_line == unit.start_line()
            && self.unit_text == unit.raw_lines().join("\n")
    }

    /// Every model that was queried has replied.
    pub fn complete(&self) -> bool {
        self.spawned
            .iter()
            .all(|&i| self.replies.get(i).is_some_and(|r| r.is_some()))
    }

    /// Every reply is in and every one of them says keep. A failed model is
    /// not a keep: silence from one model is no grounds to bury the unit.
    pub fn unanimous_keep(&self) -> bool {
        self.complete()
            && !self.spawned.is_empty()
            && self.spawned.iter().all(|&i| {
                matches!(
                    self.replies.get(i).and_then(|r| r.as_ref()),
                    Some(PrefetchedReply { result: Ok(s), .. }) if s.action == Action::Keep
                )
            })
    }
}

pub enum Msg {
    Prs(Result<Vec<PrInfo>, String>),
    Cand(CandidateMsg),
    WholeBranchReview(WholeBranchReviewMsg),
    Fix(models::FixMsg),
    Repo(RepoMsg),
    Publish(Result<publish::Outcome, String>),
}

/// How far the review's commits have got towards the remote.
pub enum PublishState {
    Idle,
    /// A push or a `gh pr create` is in flight. Only one at a time: both
    /// routes move the same commits, and a second one launched over the first
    /// would be racing it for the branch.
    Running(&'static str),
    Done(publish::Outcome),
    Failed(String),
}

impl PublishState {
    pub fn running(&self) -> bool {
        matches!(self, PublishState::Running(_))
    }
}

/// The stacked pull request's fields, as the reviewer edits them. Filled in
/// from the plan the first time the summary screen sees it, and left alone
/// afterwards so an edit survives a repaint, a failed attempt and a trip to
/// another screen and back.
#[derive(Default)]
pub struct StackForm {
    pub branch: String,
    /// The branch the pull request targets. Prefilled from [`CraApp::stack_base`]
    /// and editable, because what a stack should sit on is not always
    /// derivable — a branch rescued off a detached review has no upstream to
    /// name the thing it was built from.
    pub base: String,
    pub title: String,
    pub body: String,
    /// Put the reviewed branch back where the remote has it once the fixes
    /// are safely on their own branch. Defaulted on when it is safe.
    pub restore: bool,
    /// The session the fields were filled in for, so a different review gets
    /// its own defaults rather than the last one's.
    filled_for: Option<i64>,
}

/// Streamed repository discovery: each repository as it is found, then one
/// `Done` per source so the picker knows when the spinner can stop.
pub enum RepoMsg {
    Found {
        seq: u64,
        repo: DiscoveredRepo,
    },
    Done {
        seq: u64,
        source: RepoSource,
        err: Option<String>,
    },
    /// A `gh repo clone` finished; `Ok` carries the new checkout's path.
    Cloned {
        slug: String,
        result: Result<String, String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RepoSource {
    Local,
    GitHub,
}

/// One model's slice of the whole-branch review.
pub enum WholeBranchReviewState {
    Idle,
    /// Running; the handle is the live view updated by the worker thread.
    Pending(ProcHandle),
    /// Stopped before it reported, because the reviewer left the summary.
    Paused(PausedCall),
    Done {
        n: usize,
        latency_ms: i64,
    },
    Failed(String),
}

/// A whole-branch review finding as the summary screen holds it: the db row id so a
/// dismissal can be written back, and who reported it.
pub struct FindingRow {
    pub id: i64,
    pub model: String,
    pub finding: Finding,
    pub dismissed: bool,
}

/// A reviewer note as the follow-up screen holds it: checked means "hand this
/// to the next fix session".
pub struct NoteRow {
    pub note: Note,
    pub checked: bool,
}

pub struct CraApp {
    pub db: Db,
    pub settings: Settings,
    pub screen: Screen,
    pub prev_screen: Screen,

    /// Every model CLI this run of the app has started, running or finished.
    /// One ledger rather than a flag per page: what makes a process lost is
    /// nobody being able to name it, and every launch site registers here.
    pub procs: ProcTable,
    /// What leaving the last screen stopped, shown until dismissed. The
    /// positive confirmation the reviewer is owed lives here.
    pub nav_notice: Option<NavNotice>,
    /// Whether the process ledger window is open.
    pub show_procs: bool,
    /// Set while the window is being held open to finish killing the model
    /// CLIs. Closing is asynchronous for exactly one reason: the threads that
    /// do the killing die with the process, so exiting the instant the close
    /// is asked for would leave the children running with nothing left to
    /// report them.
    pub closing: bool,
    /// The unit the review screen's state belongs to. Coming back to a unit
    /// that already has answers or paused calls must not re-ask the models —
    /// this is what tells a return from an advance.
    pub reviewing: Option<UnitKey>,

    pub log_lines: VecDeque<(String, String)>,
    pub status: String,

    // repo picker
    pub repo_input: String,
    pub repo_sel: usize,
    pub repo_error: Option<String>,
    pub repo: Option<RepoCtx>,

    // repo discovery
    /// Everything the last completed scan (or the cache) knows, merged across
    /// sources and sorted newest-activity-first.
    pub discovered: Vec<DiscoveredRepo>,
    /// The scan currently streaming in, kept apart so a repository deleted
    /// since last time drops out when the scan completes rather than never.
    pub scan_fresh: Vec<DiscoveredRepo>,
    /// Monotonic id for scans, so a late find from an abandoned scan cannot
    /// appear in a newer list.
    pub repo_scan_seq: u64,
    pub scanning_local: bool,
    pub scanning_gh: bool,
    pub gh_repos_error: Option<String>,
    /// When the discovery cache was last rebuilt (unix seconds).
    pub repo_cache_at: i64,
    /// Slug currently being cloned; `None` when idle.
    pub cloning: Option<String>,

    // ref picker
    pub ref_tab: RefTab,
    pub branches: Vec<BranchInfo>,
    pub prs: Vec<PrInfo>,
    pub prs_loading: bool,
    pub prs_error: Option<String>,
    pub ref_sel: usize,
    pub ref_error: Option<String>,
    /// The isolated worktree the current review runs in, when it has one.
    /// `None` means the reviewer's own checkout, which is where a working-tree
    /// or staged review has to happen and where a branch already checked out
    /// there is reviewed too. Repository *identity* is never this: the
    /// database keys on [`RepoCtx::path`], so decisions and notes made in a
    /// worktree belong to the repository that owns it.
    pub review_work: Option<String>,
    /// Something the reviewer should know about a checkout that still
    /// worked — a detached HEAD, say. Not an error: the plan is built.
    pub ref_note: Option<String>,

    // file picker
    pub plan: Option<ReviewPlan>,
    pub file_sel: usize,

    // review screen
    pub review_seq: u64,
    pub candidates: Vec<CandidateState>,
    /// Snapshot of the models that own `candidates`. Settings can be edited
    /// mid-review, but a result's name and co-author must never change slots.
    pub candidate_models: Vec<ModelConfig>,
    pub chosen: Option<Choice>,
    pub editor: String,
    pub candidate_baseline: Option<String>,
    /// The comment exactly as it sits on disk, indentation included.
    pub original_text: String,
    /// `original_text` dedented — the baseline the editor is compared against,
    /// since the editor works in dedented space.
    pub original_display: String,
    pub review_error: Option<String>,
    /// Set when a save found the unit's lines gone from disk: what now sits
    /// in their place, powering the in-place resolution panel (reload/skip).
    pub stale_unit: Option<review::StaleUnit>,
    pub focus_editor: bool,

    /// Per-model conversation ID; `None` means the model has no usable session yet.
    pub sessions: Vec<Option<String>>,
    /// Per-model record of sent and received turns.
    pub convos: Vec<Vec<Turn>>,
    pub follow_up: String,
    /// Per-model link to the follow-up question whose answer is pending:
    /// `(follow_ups row id, round)`. Taken when the answer arrives, so the
    /// suggestion row records exactly which words it was answering.
    pub pending_follow_up: Vec<Option<(i64, i64)>>,
    /// Conversation round on the current unit; the opening verdicts are 1 and
    /// each follow-up question starts the next.
    pub unit_round: i64,
    /// Source for `review_seq` and prefetch sequence ids. One counter for
    /// both, so a prefetch adopted as the current unit keeps its id and any
    /// model still running arrives as a normal reply.
    pub seq_counter: u64,
    /// Queries started ahead of the review, plus finished ones stored for units
    /// pushed back by the unanimous-keep deferral.
    pub prefetches: Vec<Prefetch>,
    pub show_prompt: Option<usize>,
    /// Evidence entry being inspected: the real file at the spot a model says
    /// it read, so the human sees the same context the verdict came from.
    pub show_evidence: Option<Evidence>,
    pub focus_follow_up: bool,

    /// Decisions applied to the working tree by `Save and Continue` but not
    /// yet committed, kept so a later `Commit and Continue` can document all
    /// of them instead of just the one that triggers the commit. Paired with
    /// the decision's db row id so that row can be back-filled with the
    /// commit sha once it is included in a commit.
    pub pending: Vec<(i64, review::PendingDecision)>,
    /// When set, every decision commits immediately instead of accumulating
    /// in `pending` — one commit per decision rather than one per batch.
    /// A per-session toggle (review screen checkbox), not persisted.
    pub commit_each: bool,

    /// Monotonic id for whole-branch reviews, so a late reply from an abandoned run
    /// cannot record findings against a newer plan.
    pub whole_branch_review_seq: u64,
    /// Parallel to `settings.models`; empty until a pass is started.
    pub whole_branch_review: Vec<WholeBranchReviewState>,
    pub findings: Vec<FindingRow>,

    /// Note being typed on the review screen, bound for the follow-up backlog.
    pub note_input: String,
    pub focus_note: bool,

    /// The follow-up backlog as loaded when the screen was opened.
    pub notes: Vec<NoteRow>,
    /// Where the follow-up screen was entered from, for Esc to return to.
    pub followup_from: Screen,
    /// Editable preamble of the fix-session prompt; the checked notes are
    /// appended to it at launch.
    pub fix_prompt: String,
    /// Settings model the *next* fix session will run on.
    pub selected_fix_model_index: usize,
    /// Model the live fix conversation is using — resuming must go back to the
    /// CLI that holds the session, wherever the picker has moved since.
    pub active_fix_model_index: usize,
    /// Monotonic id for fix-session turns, so a late reply from an abandoned
    /// launch cannot update a newer conversation.
    pub fix_seq: u64,
    pub fix_running: bool,
    /// Live view of the running fix-session turn; `None` between turns.
    pub fix_proc: Option<ProcHandle>,
    /// A fix-session turn that was stopped before it finished, so the same
    /// session can be picked back up rather than restarted from the notes.
    pub fix_paused: Option<PausedCall>,
    pub fix_error: Option<String>,
    pub fix_convo: Vec<Turn>,
    pub fix_session: Option<String>,
    pub fix_follow_up: String,

    /// The evaluation page's aggregate, rebuilt on open and when the filter
    /// moves. `None` means "stale, recompute next frame" — it is two table
    /// scans, and this screen repaints on every pointer move.
    pub eval: Option<crate::eval::Leaderboard>,
    pub eval_filter: crate::eval::Filter,
    /// Repositories with review history, for the page's scope picker.
    pub eval_repos: Vec<String>,

    // publishing a finished review
    /// Where the review's commits stand against the remote, read when the
    /// summary screen is opened and again after each publish. `None` means
    /// not read yet — a review with nothing to publish still gets a state.
    pub delivery: Option<gitio::Delivery>,
    pub publish: PublishState,
    /// The stacked pull request as the reviewer has filled it in so far, kept
    /// across repaints and across a failed attempt.
    pub stack: StackForm,

    pub tx: Sender<Msg>,
    pub rx: Receiver<Msg>,
}

impl CraApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::ui::theme::apply(&cc.egui_ctx);
        let db = Db::open().expect("failed to open sqlite database");
        let mut app = CraApp::with_db(db);
        app.offer_cwd_repo();
        app.note("app", "started");
        app
    }

    /// Construct without eframe context or environment setup.
    pub fn with_db(db: Db) -> Self {
        let settings = Settings::load(&db);
        let (tx, rx) = channel();
        let cache = discover::load_cache(&db);
        CraApp {
            db,
            settings,
            screen: Screen::RepoPicker,
            prev_screen: Screen::RepoPicker,
            procs: ProcTable::new(),
            nav_notice: None,
            show_procs: false,
            closing: false,
            reviewing: None,
            log_lines: VecDeque::new(),
            status: "pick a repository".into(),
            repo_input: String::new(),
            repo_sel: 0,
            repo_error: None,
            discovered: cache.repos,
            scan_fresh: Vec::new(),
            repo_scan_seq: 0,
            scanning_local: false,
            scanning_gh: false,
            gh_repos_error: None,
            repo_cache_at: cache.fetched_at,
            cloning: None,
            repo: None,
            ref_tab: RefTab::Branches,
            branches: Vec::new(),
            prs: Vec::new(),
            prs_loading: false,
            prs_error: None,
            ref_sel: 0,
            ref_error: None,
            ref_note: None,
            review_work: None,
            plan: None,
            file_sel: 0,
            review_seq: 0,
            candidates: Vec::new(),
            candidate_models: Vec::new(),
            chosen: None,
            editor: String::new(),
            candidate_baseline: None,
            original_text: String::new(),
            original_display: String::new(),
            review_error: None,
            stale_unit: None,
            focus_editor: false,
            sessions: Vec::new(),
            convos: Vec::new(),
            follow_up: String::new(),
            pending_follow_up: Vec::new(),
            unit_round: 1,
            seq_counter: 0,
            prefetches: Vec::new(),
            show_prompt: None,
            show_evidence: None,
            focus_follow_up: false,
            pending: Vec::new(),
            commit_each: false,
            whole_branch_review_seq: 0,
            whole_branch_review: Vec::new(),
            findings: Vec::new(),
            note_input: String::new(),
            focus_note: false,
            notes: Vec::new(),
            followup_from: Screen::Summary,
            fix_prompt: String::new(),
            selected_fix_model_index: 0,
            active_fix_model_index: 0,
            fix_seq: 0,
            eval: None,
            eval_filter: crate::eval::Filter::default(),
            eval_repos: Vec::new(),
            fix_running: false,
            fix_proc: None,
            fix_paused: None,
            fix_error: None,
            fix_convo: Vec::new(),
            fix_session: None,
            fix_follow_up: String::new(),
            delivery: None,
            publish: PublishState::Idle,
            stack: StackForm::default(),
            tx,
            rx,
        }
    }

    fn offer_cwd_repo(&mut self) {
        if let Ok(cwd) = std::env::current_dir() {
            let cwd = cwd.to_string_lossy().to_string();
            if gitio::is_git_repo(&cwd) && !self.settings.recent_repos.contains(&cwd) {
                self.settings.recent_repos.push(cwd);
            }
        }
    }

    pub fn note(&mut self, kind: &str, msg: &str) {
        self.db.log(kind, msg);
        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
        self.log_lines.push_back((ts, format!("[{kind}] {msg}")));
        while self.log_lines.len() > 250 {
            self.log_lines.pop_front();
        }
        self.status = msg.to_string();
    }

    // -- repo ---------------------------------------------------------------

    // -- repo discovery ------------------------------------------------------

    /// How long the discovery cache satisfies the picker before a fresh scan
    /// is started on its own. The refresh button ignores this.
    const REPO_CACHE_FRESH_SECS: i64 = 3600;

    /// Kick off discovery unless the cache is still fresh or a scan is
    /// already running. Called every picker frame, so it must be a cheap
    /// no-op in the common case; `force` is the refresh button.
    pub fn refresh_repos(&mut self, force: bool) {
        if self.scanning_local || self.scanning_gh || self.cloning.is_some() {
            return;
        }
        let now = chrono::Utc::now().timestamp();
        if !force && now - self.repo_cache_at < Self::REPO_CACHE_FRESH_SECS {
            return;
        }
        self.repo_scan_seq += 1;
        let seq = self.repo_scan_seq;
        self.scan_fresh.clear();
        self.gh_repos_error = None;
        self.scanning_local = true;
        self.scanning_gh = true;

        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let root = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
            discover::scan_local(&root, discover::MAX_DEPTH, |repo| {
                let _ = tx.send(Msg::Repo(RepoMsg::Found { seq, repo }));
            });
            let _ = tx.send(Msg::Repo(RepoMsg::Done {
                seq,
                source: RepoSource::Local,
                err: None,
            }));
        });

        let tx = self.tx.clone();
        let gh = self.settings.gh_path.clone();
        std::thread::spawn(move || match discover::list_github(&gh, 200) {
            Ok(repos) => {
                for repo in repos {
                    let _ = tx.send(Msg::Repo(RepoMsg::Found { seq, repo }));
                }
                let _ = tx.send(Msg::Repo(RepoMsg::Done {
                    seq,
                    source: RepoSource::GitHub,
                    err: None,
                }));
            }
            Err(e) => {
                let _ = tx.send(Msg::Repo(RepoMsg::Done {
                    seq,
                    source: RepoSource::GitHub,
                    err: Some(e),
                }));
            }
        });
        self.note("repo", "scanning for repositories");
    }

    fn handle_repo(&mut self, m: RepoMsg) {
        match m {
            RepoMsg::Found { seq, repo } => {
                if seq != self.repo_scan_seq {
                    return;
                }
                discover::merge(&mut self.scan_fresh, repo.clone());
                // Also into the visible list, so results appear as they are
                // found instead of when the whole scan completes.
                discover::merge(&mut self.discovered, repo);
                self.discovered
                    .sort_by_key(|r| std::cmp::Reverse(r.last_update));
            }
            RepoMsg::Done { seq, source, err } => {
                if seq != self.repo_scan_seq {
                    return;
                }
                match source {
                    RepoSource::Local => self.scanning_local = false,
                    RepoSource::GitHub => self.scanning_gh = false,
                }
                if let Some(e) = err {
                    // gh being missing or logged out only costs the remote
                    // half of the list; say so instead of failing the picker.
                    self.gh_repos_error = Some(truncate(&e, 160));
                    self.note("error", &format!("gh repo list: {e}"));
                }
                if !self.scanning_local && !self.scanning_gh {
                    // The fresh list replaces the merged view: a repository
                    // deleted since the cache was built drops out here.
                    self.discovered = std::mem::take(&mut self.scan_fresh);
                    self.discovered
                        .sort_by_key(|r| std::cmp::Reverse(r.last_update));
                    self.repo_cache_at = chrono::Utc::now().timestamp();
                    discover::save_cache(&self.db, &self.discovered, self.repo_cache_at);
                    self.note(
                        "repo",
                        &format!("{} repositories discovered", self.discovered.len()),
                    );
                }
            }
            RepoMsg::Cloned { slug, result } => {
                self.cloning = None;
                match result {
                    Ok(path) => {
                        self.note("repo", &format!("cloned {slug}"));
                        self.select_repo(path);
                    }
                    Err(e) => {
                        self.repo_error = Some(truncate(&e, 200));
                        self.note("error", &format!("clone {slug}: {e}"));
                    }
                }
            }
        }
    }

    /// Open a repository that only exists on GitHub by cloning it first —
    /// into the configured clone directory, or the home folder next to where
    /// the scan looks. A directory already there by that name is opened, not
    /// cloned over.
    pub fn clone_and_open(&mut self, slug: String) {
        if self.cloning.is_some() {
            return;
        }
        let root = match self.settings.clone_dir.trim() {
            "" => dirs::home_dir()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".into()),
            dir => dir.to_string(),
        };
        let name = slug.rsplit('/').next().unwrap_or(&slug).to_string();
        let dest = std::path::Path::new(&root)
            .join(&name)
            .to_string_lossy()
            .to_string();
        if std::path::Path::new(&dest).exists() {
            if gitio::is_git_repo(&dest) {
                self.select_repo(dest);
            } else {
                self.repo_error = Some(format!("{dest} exists and is not a git repository"));
            }
            return;
        }
        self.cloning = Some(slug.clone());
        self.repo_error = None;
        self.note("repo", &format!("cloning {slug} into {dest}"));
        let gh = self.settings.gh_path.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = gitio::run(&root, &gh, &["repo", "clone", &slug, &dest]).map(|_| dest);
            let _ = tx.send(Msg::Repo(RepoMsg::Cloned { slug, result }));
        });
    }

    pub fn select_repo(&mut self, path: String) {
        let path = path.trim().to_string();
        if !gitio::is_git_repo(&path) {
            self.repo_error = Some(format!("not a git repository: {path}"));
            return;
        }
        self.repo_error = None;
        self.settings.remember_repo(&path);
        self.settings.save(&self.db);
        let default_branch = gitio::default_branch(&path, &self.settings.default_base);
        self.repo = Some(RepoCtx {
            name: gitio::repo_name(&path),
            path: path.clone(),
            default_branch,
        });
        // Another repository's review worktree is nothing to this one.
        self.work_in_repo();
        self.note("repo", &format!("selected {path}"));
        self.load_refs();
        self.ref_sel = 0;
        self.ref_tab = RefTab::Branches;
        self.goto(Screen::RefPicker);
    }

    pub fn load_refs(&mut self) {
        let Some(repo) = &self.repo else { return };
        let path = repo.path.clone();
        match gitio::local_branches(&path) {
            Ok(b) => self.branches = b,
            Err(e) => {
                self.branches = Vec::new();
                self.note("error", &format!("branch list: {e}"));
            }
        }
        self.prs.clear();
        self.prs_error = None;
        self.prs_loading = true;
        let gh = self.settings.gh_path.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = gitio::open_prs(&path, &gh);
            let _ = tx.send(Msg::Prs(res));
        });
    }

    // -- ref selection → plan ----------------------------------------------

    /// Where the code under review actually sits — see [`CraApp::review_work`].
    /// Every git command, file read, edit and model CLI runs here; only the
    /// database is told the repository's own path.
    pub fn work_dir(&self) -> Option<String> {
        let repo = self.repo.as_ref()?;
        Some(
            self.review_work
                .clone()
                .unwrap_or_else(|| repo.path.clone()),
        )
    }

    fn work_dir_or_default(&self) -> String {
        self.work_dir().unwrap_or_default()
    }

    /// Bring the review back to the reviewer's own checkout. What is in the
    /// worktree stays there; nothing here is reviewing it any more.
    fn work_in_repo(&mut self) {
        self.review_work = None;
    }

    pub fn select_branch(&mut self, branch: &str) {
        let Some(repo) = &self.repo else { return };
        let path = repo.path.clone();
        let default = repo.default_branch.clone();
        let cur = gitio::current_branch(&path).unwrap_or_default();
        self.ref_note = None;
        // Already standing on it: review it where it is, so a fix commit lands
        // on the branch and nothing about the checkout changes. Any other
        // branch gets a worktree of its own rather than moving a checkout the
        // reviewer is in the middle of using.
        if cur == branch {
            self.work_in_repo();
        } else {
            match worktree::for_branch(&path, branch) {
                Ok(ready) => {
                    self.ref_note = Some(ready.describe(branch));
                    self.review_work = Some(ready.path);
                }
                Err(e) => {
                    self.ref_error = Some(e);
                    return;
                }
            }
        }
        let work = self.work_dir_or_default();
        let base = if branch != default {
            default
        } else if gitio::is_dirty(&work) {
            // Reviewing the default branch itself: fall back to the working-tree
            // diff when there are uncommitted changes, else review the whole
            // history so a brand-new repo still has something to show.
            String::new()
        } else {
            gitio::EMPTY_TREE.to_string()
        };
        self.build_plan(RefKind::Branch, branch.to_string(), base);
    }

    pub fn select_working_tree(&mut self) {
        // Uncommitted work only exists in the checkout the reviewer is using.
        self.work_in_repo();
        let name = self
            .repo
            .as_ref()
            .and_then(|r| gitio::current_branch(&r.path).ok())
            .unwrap_or_else(|| "HEAD".into());
        let base = if self.settings.include_untracked {
            gitio::UNTRACKED.to_string()
        } else {
            String::new()
        };
        self.build_plan(RefKind::WorkingTree, name, base);
    }

    /// Review only what `git add` has staged: the commit being prepared,
    /// judged without whatever unstaged noise sits around it.
    pub fn select_staged(&mut self) {
        self.work_in_repo();
        let name = self
            .repo
            .as_ref()
            .and_then(|r| gitio::current_branch(&r.path).ok())
            .unwrap_or_else(|| "HEAD".into());
        self.build_plan(RefKind::Staged, name, gitio::STAGED.to_string());
    }

    pub fn select_pr(&mut self, pr: &PrInfo) {
        let Some(repo) = &self.repo else { return };
        let path = repo.path.clone();
        let gh = self.settings.gh_path.clone();
        self.ref_note = None;
        // Someone else's branch is never worth moving the reviewer's checkout
        // for — unless they are already on it, in which case there is nothing
        // to move and a fix can be committed straight onto the PR.
        if gitio::current_branch(&path).is_ok_and(|cur| cur == pr.head_ref) {
            self.work_in_repo();
        } else {
            match worktree::for_pr(&path, &gh, pr.number) {
                Ok(ready) => {
                    self.ref_note = Some(ready.describe(&format!("PR #{}", pr.number)));
                    self.review_work = Some(ready.path);
                }
                Err(e) => {
                    self.ref_error = Some(e);
                    return;
                }
            }
        }
        // The base has to be the one on the server, not the local branch of
        // the same name — see [`gitio::pr_base_ref`].
        let base = gitio::pr_base_ref(&self.work_dir_or_default(), &pr.base_ref);
        self.build_plan(RefKind::Pr(pr.number), pr.head_ref.clone(), base);
    }

    fn build_plan(&mut self, kind: RefKind, ref_name: String, base: String) {
        let Some(repo) = &self.repo else { return };
        // Two different questions: `work` is where the code is, `path` is which
        // repository it belongs to. A review run in an isolated worktree still
        // reads and writes its decisions under the repository's own path, so
        // they survive the worktree and count towards the next review of it.
        let path = repo.path.clone();
        let work = self.work_dir_or_default();
        let diff = match gitio::review_diff(&work, &base, self.settings.context_lines) {
            Ok(d) => d,
            Err(e) => {
                self.ref_error = Some(e);
                return;
            }
        };
        let branch_base = if base.is_empty() {
            gitio::head_sha(&work).unwrap_or_default()
        } else {
            base.clone()
        };
        let files = crate::diffparse::parse(&diff);
        let (want_comments, want_code) = (self.settings.review_comments, self.settings.review_code);
        let mut extracted = units::assemble(
            &work,
            &files,
            self.settings.context_lines,
            want_comments,
            want_code,
            gitio::new_side(&base),
        );
        if self.settings.triage_order {
            triage::order_riskiest_first(&mut extracted);
        }
        // A decision is meant to stick. Everything this repository has already
        // been asked about leaves the plan before it is built, so reopening
        // the app resumes the review rather than restarting it.
        let skipped = if self.settings.skip_decided {
            units::drop_decided(&mut extracted, &self.db.decided_units(&path))
        } else {
            0
        };
        if extracted.is_empty() {
            // Every unit already judged is a *finished* review, not an error:
            // the fix commits it made may still be sitting in the worktree
            // with nowhere to go, and the summary screen is the only place
            // that can send them. So it opens there, on a plan with no units
            // and the session those commits were made in — which is what lets
            // publishing prove the branch is safe to rewind.
            if skipped > 0 {
                // By name first, then by the commits themselves — a branch
                // rescued off a detached review (see [`crate::worktree`])
                // carries a finished session's work under a name that session
                // never had. Only the tip is worth asking about: review
                // commits are the newest things on the branch.
                let session = self.db.last_session(&path, &ref_name).or_else(|| {
                    gitio::delivery_state(&work, &ref_name, &base, &[])
                        .unpushed
                        .iter()
                        .take(50)
                        .find_map(|sha| self.db.session_for_commit(sha))
                });
                if let Some(session_id) = session {
                    self.adopt_plan(ReviewPlan {
                        session_id,
                        ref_kind: kind,
                        ref_name,
                        base_ref: gitio::base_label(&base).to_string(),
                        branch_base,
                        files: Vec::new(),
                        file_idx: 0,
                        unit_idx: 0,
                        decided_total: skipped,
                        skipped_decided: skipped,
                    });
                    self.goto(Screen::Summary);
                    return;
                }
            }
            let what = match (want_comments, want_code) {
                (true, true) => "units",
                (true, false) => "comment hunks",
                (false, true) => "code changes",
                (false, false) => "units — both review kinds are off in settings",
            };
            self.ref_error = Some(if skipped > 0 {
                // Only reachable with no session on record for the ref — an
                // older database, or decisions carried over from another name.
                format!(
                    "nothing left to review in {} (base: {}) — all {skipped} unit(s) already \
                     decided, and no review session on record to deliver. Untick \"skip \
                     decided\" in settings to review them again.",
                    ref_name,
                    gitio::base_label(&base)
                )
            } else {
                format!(
                    "no reviewable {what} in {} (base: {})",
                    ref_name,
                    gitio::base_label(&base)
                )
            });
            return;
        }
        // These belong to the plan being left, not merely to a relative file
        // path that a later repository or session might happen to share.
        self.pending.clear();
        self.commit_each = false;
        let session_id =
            self.db
                .new_session(&path, &kind.label(), &ref_name, gitio::base_label(&base));
        let n_comments: usize = extracted
            .iter()
            .map(|(_, u)| u.iter().filter(|u| !u.is_code()).count())
            .sum();
        let n_code: usize = extracted
            .iter()
            .map(|(_, u)| u.iter().filter(|u| u.is_code()).count())
            .sum();
        // Measurements for the picker: what the diff did to each file, and how
        // big the file is that it did it to. Taken here because this is the
        // only place that holds both the parsed diff and the checkout.
        let review_files = extracted
            .into_iter()
            .map(|(file_path, units)| {
                let mut f = ReviewFile::new(file_path, units);
                if let Some(d) = files.iter().find(|d| d.path == f.path) {
                    f.line_changes = d.line_changes();
                }
                f.total_lines = review::file_line_count(&path, &f.path);
                f
            })
            .collect::<Vec<_>>();
        let already = if skipped > 0 {
            format!(", {skipped} already decided")
        } else {
            String::new()
        };
        self.note(
            "session",
            &format!(
                "#{session_id} {} — {} unit(s) ({n_comments} comment, {n_code} code{already}) \
                 in {} files",
                ref_name,
                n_comments + n_code,
                review_files.len()
            ),
        );
        self.adopt_plan(ReviewPlan {
            session_id,
            ref_kind: kind,
            ref_name,
            base_ref: gitio::base_label(&base).to_string(),
            branch_base,
            files: review_files,
            file_idx: 0,
            unit_idx: 0,
            decided_total: 0,
            skipped_decided: skipped,
        });
        self.goto(Screen::FilePicker);
    }

    /// Take a freshly built plan, and clear everything the last one owned.
    ///
    /// A new plan invalidates any whole-branch review, running or done — and
    /// any prefetch: its answers were logged under the old session, and a
    /// decision made in this one must not be joined to them. The publish state
    /// goes too, so a finished push is not still being reported over a review
    /// of something else.
    fn adopt_plan(&mut self, plan: ReviewPlan) {
        self.plan = Some(plan);
        self.whole_branch_review_seq += 1;
        self.whole_branch_review.clear();
        self.findings.clear();
        self.prefetches.clear();
        self.ref_error = None;
        self.file_sel = 0;
        self.publish = PublishState::Idle;
        // Read here rather than leaving it to `goto`: a plan built while the
        // summary is already on screen — finishing a review, then opening the
        // same ref again — never moves screens, so `goto` returns early and
        // the state would stay as the last plan left it.
        self.refresh_delivery();
    }

    // -- review -------------------------------------------------------------

    pub fn start_review(&mut self, ctx: &egui::Context, file_idx: usize) {
        if let Some(plan) = &mut self.plan {
            plan.jump_to_file(file_idx);
            // Skip empty files (shouldn't happen; files always have units).
            if plan.current().is_none() {
                self.goto(Screen::Summary);
                return;
            }
        }
        self.goto(Screen::Review);
        self.enter_unit(ctx);
    }

    /// Start re-judging comments that were decided before. Comparing a
    /// reviewer against their own earlier self is the only way to know how
    /// much of a model's disagreement is noise rather than error — without it
    /// an agreement score has no scale to be read against.
    pub fn start_recheck(&mut self, ctx: &egui::Context, limit: usize) {
        // Past decisions span the repository's history, not an isolated
        // delivery branch, so re-check them in the source checkout.
        self.work_in_repo();
        self.ref_note = None;
        let repo_path = self
            .repo
            .as_ref()
            .map(|r| r.path.clone())
            .unwrap_or_default();
        let corpus = crate::eval::Corpus::from_db_for_repo(&self.db, &repo_path, limit);
        if corpus.entries.is_empty() {
            self.ref_error = Some(
                "no past decisions in this repository to re-check yet — review some units first"
                    .into(),
            );
            return;
        }
        // Group by file so the plan looks like any other review.
        let mut files: Vec<ReviewFile> = Vec::new();
        for entry in corpus.entries {
            match files.iter_mut().find(|f| f.path == entry.unit.file()) {
                Some(f) => f.units.push(entry.unit),
                None => files.push(ReviewFile::new(
                    entry.unit.file().to_string(),
                    vec![entry.unit],
                )),
            }
        }
        let n_units: usize = files.iter().map(|f| f.units.len()).sum();
        self.pending.clear();
        self.commit_each = false;
        let session_id = self
            .db
            .new_session(&repo_path, "re-check", "past decisions", "n/a");
        self.note(
            "session",
            &format!("#{session_id} re-check — {n_units} past decision(s)"),
        );
        self.plan = Some(ReviewPlan {
            session_id,
            ref_kind: RefKind::Recheck,
            ref_name: "past decisions".into(),
            base_ref: "n/a".into(),
            branch_base: "n/a".into(),
            files,
            file_idx: 0,
            unit_idx: 0,
            decided_total: 0,
            // A re-check is *about* re-judging decided units; nothing was held back.
            skipped_decided: 0,
        });
        self.prefetches.clear();
        self.ref_error = None;
        self.file_sel = 0;
        self.start_review(ctx, 0);
    }

    /// The next id for a review position or a prefetch. Shared, so a reply
    /// always names exactly one request whenever its reply arrives.
    fn next_seq(&mut self) -> u64 {
        self.seq_counter += 1;
        self.seq_counter
    }

    /// The prompt for a unit: the reviewer's standing preferences (when
    /// enabled and either written or minable), then the unit itself.
    fn unit_prompt(&self, unit: &ReviewUnit) -> String {
        let base = unit.build_prompt();
        if !self.settings.send_profile {
            return base;
        }
        match crate::profile::preamble(&self.db, &self.settings.reviewer_preferences) {
            Some(p) => format!("{p}\n{base}"),
            None => base,
        }
    }

    /// Prepare state for the current unit and start the model CLIs — or, when
    /// a prefetch already asked them, install the answers that are in.
    /// Show the unit the review is on, querying the models about it.
    ///
    /// Arriving back at a unit whose calls were paused is the exception: those
    /// sessions are still open, and quietly replacing them with a second set
    /// would be both the loss this tracking exists to prevent and a second
    /// bill for the same verdict. The paused cards say what each model was in
    /// the middle of, and picking it back up — or asking again — is the
    /// reviewer's call. Every other arrival queries as it always did, R
    /// included: asking again is what that key is for.
    pub fn enter_unit(&mut self, ctx: &egui::Context) {
        let same_unit = self
            .plan
            .as_ref()
            .and_then(|p| p.current().map(|(_, u)| unit_key(u)))
            .is_some_and(|k| self.reviewing.as_ref() == Some(&k));
        let paused = self
            .candidates
            .iter()
            .filter(|c| matches!(c, CandidateState::Paused(_)))
            .count();
        if same_unit && paused > 0 {
            self.review_error = None;
            self.note(
                "review",
                &format!("back on the same unit — {paused} paused session(s), nothing restarted"),
            );
            return;
        }
        self.requery_unit(ctx);
    }

    /// Query the models about the current unit, whatever state it is in —
    /// the review screen's R, and what every fresh arrival does.
    pub fn requery_unit(&mut self, ctx: &egui::Context) {
        self.defer_unanimous_keeps();
        self.chosen = None;
        self.candidate_baseline = None;
        self.review_error = None;
        self.stale_unit = None;

        let Some((unit, file, line)) = self.plan.as_ref().and_then(|p| {
            p.current()
                .map(|(_, u)| (u.clone(), u.file().to_string(), u.start_line()))
        }) else {
            self.prefetches.clear();
            self.reviewing = None;
            self.goto(Screen::Summary);
            return;
        };
        self.original_text = unit.raw_lines().join("\n");
        // Comments edit flush left (their indentation is put back on save);
        // code edits exactly as it sits in the file.
        self.original_display = unit.display_text();
        self.editor = self.original_display.clone();

        let prefetch = self
            .prefetches
            .iter()
            .position(|p| p.is_for(&unit))
            .map(|pos| self.prefetches.remove(pos));
        self.candidate_models = prefetch
            .as_ref()
            .map(|p| p.models.clone())
            .unwrap_or_else(|| self.settings.models.clone());
        // Every model starts Disabled; whichever path queries it below — a
        // fresh spawn or an adopted prefetch — installs Pending along with
        // the live view of the call it is actually waiting on.
        self.candidates = self
            .candidate_models
            .iter()
            .map(|_| CandidateState::Disabled)
            .collect();
        self.convos = vec![Vec::new(); self.candidate_models.len()];
        self.sessions = vec![None; self.candidate_models.len()];
        self.pending_follow_up = vec![None; self.candidate_models.len()];
        self.unit_round = 1;
        self.follow_up.clear();
        // A half-typed note belongs to the unit it was typed on; advancing
        // must not carry it to a locus it was never about.
        self.note_input.clear();
        self.show_prompt = None;
        self.show_evidence = None;
        self.reviewing = Some(unit_key(&unit));
        if let Some(pf) = prefetch {
            self.adopt_prefetch(ctx, pf, &file, line);
        } else {
            self.spawn_unit_models(ctx, &unit);
            self.note("review", &format!("{file}:{line} — querying models"));
        }
        self.start_prefetch(ctx);
    }

    /// Start every enabled model for a unit under a fresh sequence id, which
    /// becomes the current `review_seq`.
    fn spawn_unit_models(&mut self, ctx: &egui::Context, unit: &ReviewUnit) {
        if let Some(why) = self.usage_block() {
            self.review_error = Some(why.clone());
            self.note("usage", &why);
            return;
        }
        self.review_seq = self.next_seq();
        let what = format!("{}:{} · verdict", unit.file(), unit.start_line());
        let prompt = self.unit_prompt(unit);
        // The CLIs are started here so the paths in the prompt resolve and the
        // rest of the codebase is within reach.
        let repo_path = self.work_dir_or_default();
        let cli_home = self.cli_home(&repo_path);
        let timeout = self.settings.model_timeout_secs;
        let enabled_models: Vec<_> = self
            .candidate_models
            .iter()
            .cloned()
            .enumerate()
            .filter(|(_, model)| model.enabled)
            .collect();
        for (idx, model) in enabled_models {
            let (command, session) = opening_command(&model);
            if session.is_some() {
                self.sessions[idx] = session;
            }
            self.convos[idx].push(Turn {
                prompt: prompt.clone(),
                reply: String::new(),
            });
            let proc = self.procs.register(Owner::Review, idx, &model.name, &what);
            // A session id the app generated is known before the process is:
            // recording it now is what lets a call killed seconds later still
            // be resumed into the same conversation.
            proc.set_session(self.sessions[idx].clone());
            self.candidates[idx] = CandidateState::Pending(proc.clone());
            let tx = self.tx.clone();
            models::spawn_model(
                self.review_seq,
                idx,
                model,
                command,
                prompt.clone(),
                repo_path.clone(),
                cli_home.clone(),
                timeout,
                proc,
                move |m| {
                    let _ = tx.send(Msg::Cand(m));
                },
                ctx.clone(),
            );
        }
    }

    /// Make a prefetch the current request: answers already in become ready
    /// candidates, and its sequence id becomes `review_seq` so a model still
    /// still running arrives as a normal reply. Nothing is logged to the database
    /// here — each answer was recorded when it arrived, or will be.
    fn adopt_prefetch(&mut self, ctx: &egui::Context, mut pf: Prefetch, file: &str, line: u32) {
        self.review_seq = pf.seq;
        self.sessions = pf.sessions.clone();
        let mut arrived = 0usize;
        for &idx in &pf.spawned {
            let Some(model) = self.candidate_models.get(idx) else {
                continue;
            };
            let reply = pf.replies.get_mut(idx).and_then(|r| r.take());
            let turn_reply = match &reply {
                Some(r) => reply_text(&r.result, &r.raw),
                None => String::new(),
            };
            if let Some(convo) = self.convos.get_mut(idx) {
                convo.push(Turn {
                    prompt: pf.prompt.clone(),
                    reply: turn_reply,
                });
            }
            let Some(r) = reply else {
                // Still running: show it as pending with the prefetch's own
                // live view, so the clock reads from the real start.
                if let Some(c) = self.candidates.get_mut(idx) {
                    let live = pf.lives.get(idx).cloned().flatten().unwrap_or_default();
                    *c = CandidateState::Pending(live);
                }
                continue;
            };
            arrived += 1;
            // The CLI's own session id, reported in the reply, wins over any
            // id that was generated for the command line.
            if let Some(id) = models::extract_session_id(&r.raw, &model.session_key) {
                if let Some(s) = self.sessions.get_mut(idx) {
                    *s = Some(id);
                }
            }
            if let Some(c) = self.candidates.get_mut(idx) {
                *c = match r.result {
                    Ok(s) => CandidateState::Ready(s),
                    Err(e) => CandidateState::Failed(e),
                };
            }
        }
        self.note(
            "review",
            &format!(
                "{file}:{line} — prefetched, {arrived}/{} answer(s) already in",
                pf.spawned.len()
            ),
        );
        // A model enabled since the prefetch started was never queried; ask it
        // now, under the adopted sequence, so every enabled model still finishes.
        let repo_path = self.work_dir_or_default();
        let cli_home = self.cli_home(&repo_path);
        let timeout = self.settings.model_timeout_secs;
        let enabled_models: Vec<_> = self
            .candidate_models
            .iter()
            .cloned()
            .enumerate()
            .filter(|(_, model)| model.enabled)
            .collect();
        for (idx, model) in enabled_models {
            if pf.spawned.contains(&idx) {
                continue;
            }
            let (command, session) = opening_command(&model);
            if session.is_some() {
                if let Some(s) = self.sessions.get_mut(idx) {
                    *s = session;
                }
            }
            if let Some(convo) = self.convos.get_mut(idx) {
                convo.push(Turn {
                    prompt: pf.prompt.clone(),
                    reply: String::new(),
                });
            }
            let what = format!("{file}:{line} · verdict");
            let proc = self.procs.register(Owner::Review, idx, &model.name, &what);
            proc.set_session(self.sessions.get(idx).cloned().flatten());
            if let Some(c) = self.candidates.get_mut(idx) {
                *c = CandidateState::Pending(proc.clone());
            }
            let tx = self.tx.clone();
            models::spawn_model(
                self.review_seq,
                idx,
                model,
                command,
                pf.prompt.clone(),
                repo_path.clone(),
                cli_home.clone(),
                timeout,
                proc,
                move |m| {
                    let _ = tx.send(Msg::Cand(m));
                },
                ctx.clone(),
            );
        }
    }

    /// Start the models for the unit after the current one, so their verdicts
    /// are waiting when the review advances. One prefetch runs at a time —
    /// the review only ever needs the next unit, so extra concurrent calls do not help.
    fn start_prefetch(&mut self, ctx: &egui::Context) {
        if !self.settings.prefetch_next {
            return;
        }
        let Some(next) = self.plan.as_ref().and_then(|p| p.peek_next()).cloned() else {
            return;
        };
        if self.prefetches.iter().any(|p| p.is_for(&next)) {
            return;
        }
        if self.prefetches.iter().any(|p| !p.complete()) {
            return;
        }
        if self.usage_block().is_some() {
            return;
        }
        let session_id = self.plan.as_ref().map(|p| p.session_id).unwrap_or(0);
        let seq = self.next_seq();
        let what = format!("{}:{} · prefetch", next.file(), next.start_line());
        let prompt = self.unit_prompt(&next);
        let repo_path = self.work_dir_or_default();
        let cli_home = self.cli_home(&repo_path);
        let timeout = self.settings.model_timeout_secs;
        let models = self.settings.models.clone();
        let n = models.len();
        let mut sessions = vec![None; n];
        let mut spawned = Vec::new();
        let mut lives: Vec<Option<ProcHandle>> = vec![None; n];
        for (idx, model) in models
            .iter()
            .cloned()
            .enumerate()
            .filter(|(_, m)| m.enabled)
        {
            let (command, session) = opening_command(&model);
            if session.is_some() {
                sessions[idx] = session;
            }
            let proc = self.procs.register(Owner::Review, idx, &model.name, &what);
            proc.set_session(sessions[idx].clone());
            lives[idx] = Some(proc.clone());
            let tx = self.tx.clone();
            models::spawn_model(
                seq,
                idx,
                model,
                command,
                prompt.clone(),
                repo_path.clone(),
                cli_home.clone(),
                timeout,
                proc,
                move |m| {
                    let _ = tx.send(Msg::Cand(m));
                },
                ctx.clone(),
            );
            spawned.push(idx);
        }
        if spawned.is_empty() {
            return;
        }
        self.prefetches.push(Prefetch {
            seq,
            session_id,
            file: next.file().to_string(),
            start_line: next.start_line(),
            end_line: next.end_line(),
            unit_text: next.raw_lines().join("\n"),
            prompt,
            models,
            sessions,
            spawned,
            replies: (0..n).map(|_| None).collect(),
            lives,
            deferred: false,
        });
    }

    /// While the unit the review is about to offer has a complete prefetch with
    /// every model saying keep, push it to the end of its file — the units the
    /// models disagree about are the ones worth attention first. Each unit is
    /// deferred at most once, so the loop always terminates and every unit is
    /// still reviewed.
    fn defer_unanimous_keeps(&mut self) {
        if !self.settings.defer_unanimous_keeps {
            return;
        }
        if self.plan.as_ref().is_none_or(|p| p.is_recheck()) {
            return;
        }
        loop {
            let Some(unit) = self
                .plan
                .as_ref()
                .and_then(|p| p.current().map(|(_, u)| u.clone()))
            else {
                return;
            };
            let Some(pos) = self
                .prefetches
                .iter()
                .position(|p| p.is_for(&unit) && !p.deferred && p.unanimous_keep())
            else {
                return;
            };
            if !self.plan.as_mut().is_some_and(|p| p.defer_current()) {
                return;
            }
            self.prefetches[pos].deferred = true;
            self.note(
                "triage",
                &format!(
                    "{}:{} — every model says keep; deferred to the end of the file",
                    unit.file(),
                    unit.start_line()
                ),
            );
        }
    }

    /// Prepare the app-managed CLI home, if any enabled model asks for one.
    ///
    /// Only a template carrying `{cli_home}` needs it, so a user who has
    /// pointed a model at their own configured CLI is left alone. A failure to
    /// write it is not fatal: the CLI falls back to its own defaults, which is
    /// how it behaved before this existed.
    fn cli_home(&mut self, repo: &str) -> String {
        let wanted = self.settings.enabled_models().iter().any(|(_, m)| {
            m.command.contains("{cli_home}") || m.resume_command.contains("{cli_home}")
        });
        if !wanted {
            return String::new();
        }
        match crate::agycli::configure(repo) {
            Ok(home) => home.to_string_lossy().to_string(),
            Err(e) => {
                self.note("agy", &format!("could not write CLI permissions: {e}"));
                String::new()
            }
        }
    }

    fn fix_cli_home(&mut self, repo: &str, command: &str) -> String {
        if !command.contains("{cli_home}") {
            return String::new();
        }
        match crate::agycli::configure_fix(repo) {
            Ok(home) => home.to_string_lossy().to_string(),
            Err(e) => {
                self.note(
                    "agy",
                    &format!("could not write fix-session permissions: {e}"),
                );
                String::new()
            }
        }
    }

    /// A model can take a follow-up once its previous request has come back and
    /// it has a session to resume. Waiting for the reply keeps one answer per
    /// request, so a late one can never be misfiled against the wrong turn.
    pub fn can_ask(&self, model_index: usize) -> bool {
        let reply_ready = matches!(
            self.candidates.get(model_index),
            Some(CandidateState::Ready(_)) | Some(CandidateState::Failed(_))
        );
        let resumable = self
            .candidate_models
            .get(model_index)
            .is_some_and(|m| !m.resume_command.trim().is_empty());
        reply_ready && resumable && self.sessions.get(model_index).is_some_and(|s| s.is_some())
    }

    /// Send the pending follow-up to one model, or to every model that can take
    /// it. Each goes out on the CLI's own resumed session, so only the new
    /// message travels — the model still has the rest of the conversation.
    /// The question is recorded in full before anything is sent, and every
    /// answer will carry its row id: the question and its answers are one
    /// conversation, and the record keeps them one.
    pub fn ask_followup(&mut self, ctx: &egui::Context, model: Option<usize>) {
        let message = self.follow_up.trim().to_string();
        if message.is_empty() {
            return;
        }
        if let Some(why) = self.usage_block() {
            self.review_error = Some(why.clone());
            self.note("usage", &why);
            return;
        }
        let targets: Vec<usize> = match model {
            Some(i) => vec![i],
            None => (0..self.candidates.len()).collect(),
        };
        let targets: Vec<usize> = targets.into_iter().filter(|&i| self.can_ask(i)).collect();
        if targets.is_empty() {
            self.review_error =
                Some("no model has a resumable session ready for a follow-up yet".into());
            return;
        }
        let (file, start, end) = self
            .current_unit()
            .map(|u| (u.file().to_string(), u.start_line(), u.end_line()))
            .unwrap_or_default();
        let session_id = self.plan.as_ref().map(|p| p.session_id).unwrap_or(0);
        self.unit_round += 1;
        let follow_up_id =
            self.db
                .log_follow_up(session_id, &file, start, end, self.unit_round, &message);
        let repo_path = self.work_dir_or_default();
        let cli_home = self.cli_home(&repo_path);
        let timeout = self.settings.model_timeout_secs;
        let is_code = self.current_unit().map(|u| u.is_code()).unwrap_or(false);
        let mut sent: Vec<usize> = Vec::new();
        for idx in targets {
            let Some(model_config) = self.candidate_models.get(idx).cloned() else {
                continue;
            };
            let Some(session) = self.sessions[idx].clone() else {
                continue;
            };
            let command = model_config.resume_command.replace("{session}", &session);
            let prompt = models::followup_prompt(&message, is_code);
            self.convos[idx].push(Turn {
                prompt: prompt.clone(),
                reply: String::new(),
            });
            let what = format!("{file}:{start} · follow-up {}", self.unit_round);
            let proc = self
                .procs
                .register(Owner::Review, idx, &model_config.name, &what);
            proc.set_session(Some(session.clone()));
            self.candidates[idx] = CandidateState::Pending(proc.clone());
            if let Some(p) = self.pending_follow_up.get_mut(idx) {
                *p = Some((follow_up_id, self.unit_round));
            }
            let tx = self.tx.clone();
            models::spawn_model(
                self.review_seq,
                idx,
                model_config.clone(),
                command,
                prompt,
                repo_path.clone(),
                cli_home.clone(),
                timeout,
                proc,
                move |m| {
                    let _ = tx.send(Msg::Cand(m));
                },
                ctx.clone(),
            );
            sent.push(idx);
        }
        self.follow_up.clear();
        // Named by display position while blinded: which models were asked is
        // as much of a tell as which one answered.
        let order = self.candidate_order();
        sent.sort_by_key(|i| order.iter().position(|s| s == i).unwrap_or(*i));
        let who = sent
            .iter()
            .map(|&i| self.model_display(i))
            .collect::<Vec<_>>()
            .join(", ");
        self.note(
            "follow-up",
            &format!("asked {who}: {}", truncate(&message, 80)),
        );
    }

    /// Display position -> model index for the current comment. Identity when
    /// blinding is off; a stable shuffle when it is on.
    pub fn candidate_order(&self) -> Vec<usize> {
        let n = self.candidates.len();
        if !self.settings.blind_review {
            return (0..n).collect();
        }
        match self.current_unit() {
            Some(u) => review::blind_order(review::unit_seed(u.file(), u.start_line()), n),
            None => (0..n).collect(),
        }
    }

    /// Whether model identities are currently hidden. Once a blind choice
    /// reveals them, the choice is locked until it is saved.
    pub fn names_hidden(&self) -> bool {
        self.settings.blind_review && !matches!(self.chosen, Some(Choice::Candidate(_)))
    }

    /// What to call model `idx` at display position `pos` right now.
    pub fn model_label(&self, idx: usize, pos: usize) -> String {
        if self.names_hidden() {
            format!("model {}", (b'A' + pos as u8) as char)
        } else {
            self.candidate_models
                .get(idx)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| format!("model {idx}"))
        }
    }

    /// What to call a model in a view the reviewer did not open on purpose:
    /// the stop banner, which appears by itself after a navigation.
    ///
    /// The candidate cards deliberately carry no pid, session or activity
    /// while blinded, so nothing pairs them with a ledger row by identifier.
    /// The banner is the remaining hole: with one call left running it names
    /// exactly one model beside exactly one paused card, and that is a pairing
    /// however carefully the card itself is written. Views the reviewer opens
    /// deliberately — the process ledger, the prompt inspector — keep the real
    /// names, as the inspector always has.
    pub fn unbidden_model_label(&self, owner: Owner, model_index: usize, model: &str) -> String {
        if owner != Owner::Review || !self.names_hidden() {
            return model.to_string();
        }
        self.model_display(model_index)
    }

    /// What to call model `idx` in prose — activity lines, follow-up echoes.
    /// Resolves the display position itself so log text honours blinding the
    /// same way the candidate cards do.
    pub fn model_display(&self, idx: usize) -> String {
        let pos = self
            .candidate_order()
            .iter()
            .position(|&s| s == idx)
            .unwrap_or(idx);
        self.model_label(idx, pos)
    }

    pub fn current_unit(&self) -> Option<ReviewUnit> {
        self.plan
            .as_ref()
            .and_then(|p| p.current().map(|(_, u)| u.clone()))
    }

    pub fn choose_candidate(&mut self, model_index: usize) {
        if self.settings.blind_review && matches!(self.chosen, Some(Choice::Candidate(_))) {
            return;
        }
        let Some(CandidateState::Ready(s)) = self.candidates.get(model_index) else {
            return;
        };
        let s = s.clone();
        let Some(unit) = self.current_unit() else {
            return;
        };
        let text = match s.action {
            // A flag proposes no text: picking it endorses the concern, and
            // the unit's lines stay as they are.
            Action::Keep | Action::Flag => self.original_display.clone(),
            Action::Delete => String::new(),
            Action::Rewrite => unit.replacement_display(&s.comment),
        };
        self.editor = text.clone();
        self.candidate_baseline = Some(text);
        self.chosen = Some(Choice::Candidate(model_index));
        // Named, not lettered: `chosen` is set above, so blinding has lifted
        // and the pick is on the record under the model that made it.
        let name = self.model_display(model_index);
        self.note(
            "choice",
            &format!(
                "picked {name} ({})",
                units::action_label(s.action, unit.kind())
            ),
        );
    }

    pub fn choose_keep(&mut self) {
        if self.settings.blind_review && matches!(self.chosen, Some(Choice::Candidate(_))) {
            return;
        }
        self.editor = self.original_display.clone();
        self.candidate_baseline = None;
        self.chosen = Some(Choice::KeepOriginal);
        self.note("choice", "keep original");
    }

    pub fn choose_delete(&mut self) {
        if self.settings.blind_review && matches!(self.chosen, Some(Choice::Candidate(_))) {
            return;
        }
        self.editor.clear();
        self.candidate_baseline = None;
        self.chosen = Some(Choice::Delete);
        self.note("choice", "delete comment");
    }

    /// The action the current editor state implies. A picked flag candidate
    /// with the text left untouched records a flag — the text did not change,
    /// but "unchanged and endorsed as fine" and "unchanged and endorsed as
    /// worrying" are different verdicts.
    pub fn current_action(&self) -> Action {
        let action = review::final_action(&self.editor, &self.original_display);
        if action == Action::Keep {
            if let Some(Choice::Candidate(i)) = &self.chosen {
                if let Some(CandidateState::Ready(s)) = self.candidates.get(*i) {
                    if s.action == Action::Flag {
                        return Action::Flag;
                    }
                }
            }
        }
        action
    }

    /// Commit the entry's file with a message covering it plus every decision
    /// still pending on the same file — otherwise a batch of `Save and
    /// Continue`s would be included in one commit whose message only covers the
    /// decision that happened to trigger it. Returns the commit sha, or
    /// records the error (the caller abandons the save) and returns None.
    fn commit_decision(
        &mut self,
        repo_path: &str,
        entry: review::PendingDecision,
    ) -> Option<String> {
        let file = entry.file.clone();
        let staged = self
            .plan
            .as_ref()
            .is_some_and(|p| p.ref_kind == RefKind::Staged);
        let (ids, mut entries): (Vec<i64>, Vec<review::PendingDecision>) = self
            .pending
            .iter()
            .filter(|(_, p)| staged || p.file == file)
            .cloned()
            .unzip();
        entries.push(entry);
        let msg = review::commit_message_batch(&entries);
        let result = if staged {
            gitio::commit_index(repo_path, &msg)
        } else {
            gitio::stage_and_commit(repo_path, &file, &msg)
        };
        match result {
            Ok(s) => {
                self.note(
                    "commit",
                    &format!(
                        "{} {} ({} decision{})",
                        &s[..8.min(s.len())],
                        file,
                        entries.len(),
                        if entries.len() == 1 { "" } else { "s" }
                    ),
                );
                for id in &ids {
                    self.db.mark_committed(*id, &s);
                }
                self.pending.retain(|(_, p)| !staged && p.file != file);
                Some(s)
            }
            Err(e) => {
                self.review_error = Some(e.clone());
                self.note("error", &e);
                None
            }
        }
    }

    /// Shared save/commit path. Applies the editor content to the working
    /// tree, validates it if a check command is configured, logs the
    /// decision, optionally commits, then advances.
    pub fn save_and_continue(&mut self, ctx: &egui::Context, commit: bool) {
        let Some(unit) = self.current_unit() else {
            return;
        };
        let Some(repo_path) = self.work_dir() else {
            return;
        };

        let action = self.current_action();
        let chosen_model = match &self.chosen {
            Some(Choice::Candidate(i)) => self.candidate_models.get(*i).map(|m| {
                (
                    m.name.clone(),
                    m.coauthor.clone(),
                    m.model.clone(),
                    m.effort.clone(),
                )
            }),
            _ => None,
        };
        let mut provenance = review::derive_provenance(
            &self.chosen,
            chosen_model
                .as_ref()
                .map(|(n, c, _, _)| (n.as_str(), c.as_str())),
            &self.editor,
            self.candidate_baseline.as_deref(),
            &self.original_display,
        );
        // A flag leaves the text untouched, which derive_provenance reads as
        // "original" — but the judgement is the model's, and that is what the
        // record should say.
        if action == Action::Flag {
            if let Some((n, c, _, _)) = &chosen_model {
                provenance = review::Provenance::Model {
                    name: n.clone(),
                    coauthor: c.clone(),
                    edited: false,
                };
            }
        }
        let model_info: Option<(String, String)> = chosen_model
            .as_ref()
            .map(|(_, _, model, effort)| (model.clone(), effort.clone()));
        let justification = match &self.chosen {
            Some(Choice::Candidate(i)) => match self.candidates.get(*i) {
                Some(CandidateState::Ready(s)) => Some(s.justification.clone()),
                _ => None,
            },
            _ => None,
        };

        // Apply to the working tree when the text changed. A re-check is
        // measuring the reviewer, not editing code, so it stops short of this
        // — and a keep or a flag has nothing to write.
        let recheck = self.plan.as_ref().is_some_and(|p| p.is_recheck());
        let staged = self
            .plan
            .as_ref()
            .is_some_and(|p| p.ref_kind == RefKind::Staged);
        let makes_edit = matches!(action, Action::Rewrite | Action::Delete)
            && !recheck
            && !unit.is_deleted_file();
        let new_lines = unit.editor_to_lines(&self.editor);
        let final_text = new_lines.join("\n");
        let mut delta = 0i64;
        // How far the unit had moved beyond what the recorded edits explain,
        // measured if the save had to relocate. Recorded alongside the edit
        // so later units in the file start from a corrected hint.
        let mut drift = 0i64;
        if makes_edit {
            let Some(plan) = &self.plan else { return };
            let file = &plan.files[plan.file_idx];
            let expected_start0 =
                (unit.start_line() as i64 - 1 + file.offset_for(unit.start_line())).max(0) as usize;
            // Where the edit was actually applied — the revert below must aim here,
            // not at where the plan thought the unit was.
            let mut applied_start0 = expected_start0;
            let applied = if staged {
                review::apply_edit_to_index(&repo_path, file, &unit, &new_lines)
            } else {
                review::apply_edit(&repo_path, file, &unit, &new_lines)
            };
            match applied {
                Ok(d) => delta = d,
                Err(first_err) => {
                    if staged {
                        self.review_error = Some(first_err.clone());
                        self.note("error", &first_err);
                        return;
                    }
                    // The file changed on disk since the diff was taken. If
                    // the unit's lines merely moved, apply the edit where they
                    // sit now; if they are gone, put the resolution in the
                    // human's hands (reload from disk / skip) instead of
                    // leaving a dead end.
                    match review::find_unit_on_disk(
                        &repo_path,
                        unit.file(),
                        unit.raw_lines(),
                        expected_start0,
                    ) {
                        Ok(review::CurrentUnitLocation::Moved(start0)) => {
                            match review::splice_lines(
                                &repo_path,
                                unit.file(),
                                start0,
                                unit.raw_lines(),
                                &new_lines,
                            ) {
                                Ok(d) => {
                                    delta = d;
                                    drift = start0 as i64 - expected_start0 as i64;
                                    applied_start0 = start0;
                                    self.note(
                                    "relocate",
                                    &format!(
                                        "{}:{} drifted {drift:+} line(s) on disk — edit relocated at line {}",
                                        unit.file(),
                                        unit.start_line(),
                                        start0 + 1
                                    ),
                                );
                                }
                                Err(e) => {
                                    self.review_error = Some(e.clone());
                                    self.note("error", &e);
                                    return;
                                }
                            }
                        }
                        Ok(review::CurrentUnitLocation::Changed(stale)) => {
                            self.stale_unit = Some(stale);
                            self.review_error = Some(first_err.clone());
                            self.note("error", &first_err);
                            return;
                        }
                        Err(e) => {
                            self.review_error = Some(first_err.clone());
                            self.note(
                                "error",
                                &format!("{first_err}; relocating also failed: {e}"),
                            );
                            return;
                        }
                    }
                }
            }
            // Validate the tree still passes the repo's own check before the
            // edit is allowed to stand. A failing edit is reverted on the
            // spot: better to lose a rewrite than to review on over a break.
            let check = self.settings.check_command.trim().to_string();
            if !staged
                && !check.is_empty()
                && (unit.is_code() || self.settings.validate_comment_edits)
            {
                self.note("check", &format!("running `{check}`…"));
                let timeout =
                    std::time::Duration::from_secs(self.settings.check_timeout_secs.max(5));
                if let Err(e) = review::run_check(&repo_path, &check, timeout) {
                    match review::splice_lines(
                        &repo_path,
                        unit.file(),
                        applied_start0,
                        &new_lines,
                        unit.raw_lines(),
                    ) {
                        Ok(_) => {
                            self.review_error =
                                Some(format!("edit reverted — {}", truncate(&e, 600)));
                            self.note("check", "failed — edit reverted");
                        }
                        Err(revert_err) => {
                            // Should be unreachable: nothing else touched the
                            // file between apply and revert. Say so loudly.
                            self.review_error = Some(format!(
                                "check failed AND the revert failed ({revert_err}) — \
                                 inspect the working tree before continuing. Check said: {}",
                                truncate(&e, 400)
                            ));
                            self.note("error", "check failed and revert failed");
                        }
                    }
                    return;
                }
                self.note("check", "passed");
            }
        }

        // Commit if asked (or if the "commit each decision" toggle is on) and
        // there is something to commit.
        let commit = commit || self.commit_each;
        let mut sha = None;
        let mut committed = false;
        let mut commit_error = None;
        // Set when this decision itself stays uncommitted, so it can be added
        // to `self.pending` once its db row id is known below.
        let mut defer: Option<review::PendingDecision> = None;
        // A keep normally has nothing to commit — the kept lines already sit
        // in a commit. Not so in a working-tree or staged review: there the
        // approved lines exist nowhere but the uncommitted file, so a commit
        // was asked for and must happen, or an untracked file stays untracked
        // however many of its hunks are approved.
        let keep_commits = commit
            && action == Action::Keep
            && self.plan.as_ref().is_some_and(|p| p.reviews_uncommitted());
        if makes_edit || keep_commits {
            let entry = review::PendingDecision {
                file: unit.file().to_string(),
                line: unit.start_line(),
                kind: unit.kind(),
                action,
                provenance: provenance.clone(),
                justification: justification.clone(),
                model_info: model_info.clone(),
            };
            if commit {
                // Commits are per file, not per hunk, so an earlier decision
                // on this file may have already swept this one's lines into
                // its commit — then there is genuinely nothing left to do.
                let nothing_to_commit = if staged {
                    !gitio::index_is_dirty(&repo_path)
                } else {
                    !gitio::file_is_dirty(&repo_path, unit.file())
                };
                if keep_commits && nothing_to_commit {
                    self.note("commit", "file already committed — nothing left to commit");
                } else {
                    match self.commit_decision(&repo_path, entry.clone()) {
                        Some(s) => {
                            committed = true;
                            sha = Some(s);
                        }
                        None => {
                            // The edit is already applied (and may already be
                            // staged). Record it and advance so retrying cannot
                            // apply the same unit at stale line numbers.
                            commit_error = self.review_error.take();
                            defer = Some(entry);
                        }
                    }
                }
            } else {
                defer = Some(entry);
            }
        } else if commit && action == Action::Keep {
            self.note("commit", "kept original — nothing to commit");
        } else if commit && action == Action::Flag {
            self.note(
                "commit",
                "flag recorded — nothing changed, nothing to commit",
            );
        }

        let session_id = self.plan.as_ref().map(|p| p.session_id).unwrap_or(0);
        // Store the unit itself so this judgement can be replayed against a
        // different model later without needing the repository to still exist.
        let unit_json = serde_json::to_string(&unit).ok();
        let decision_id = self.db.log_decision(&crate::db::DecisionRecord {
            session_id,
            file: unit.file(),
            line_start: unit.start_line(),
            line_end: unit.end_line(),
            original: &self.original_text,
            action: action.as_str(),
            final_text: &final_text,
            source: &provenance.source_str(),
            human_edited: matches!(
                provenance,
                review::Provenance::Human | review::Provenance::Model { edited: true, .. }
            ),
            committed,
            commit_sha: sha.as_deref(),
            justification: justification.as_deref(),
            unit_json: unit_json.as_deref(),
            blinded: self.settings.blind_review,
        });
        if let Some(entry) = defer {
            self.pending.push((decision_id, entry));
        }
        self.note(
            "decision",
            &format!(
                "{} {}:{} ({})",
                action.as_str(),
                unit.file(),
                unit.start_line(),
                provenance.source_str()
            ),
        );

        if let Some(plan) = &mut self.plan {
            if makes_edit {
                // Drift came from external edits above this unit, which moved
                // everything below it by the same amount. Units between the
                // external edit and here that the review has yet to visit will
                // relocate themselves if this correction is not enough.
                if drift != 0 {
                    plan.files[plan.file_idx]
                        .edits
                        .push((unit.start_line(), drift));
                }
                plan.files[plan.file_idx]
                    .edits
                    .push((unit.start_line(), delta));
            }
            plan.files[plan.file_idx].decided += 1;
            plan.decided_total += 1;
            if plan.advance() {
                self.enter_unit(ctx);
            } else {
                self.goto(Screen::Summary);
                self.note("session", "review complete");
            }
        }
        if let Some(e) = commit_error {
            self.review_error = Some(e.clone());
            self.note("error", &format!("edit saved but commit failed: {e}"));
        }
    }

    /// Leave the review before it is finished. Nothing is lost: every decision
    /// already made is on the record, the plan keeps its place so the file
    /// picker resumes where this left off, and the summary opened here is
    /// also the door to the whole-branch review and the follow-up notes — neither of
    /// which needs every unit decided first.
    pub fn end_session(&mut self) {
        let progress = self
            .plan
            .as_ref()
            .map(|p| format!("{}/{} unit(s) decided", p.decided_total, p.total_units()));
        if let Some(progress) = progress {
            self.note("session", &format!("ended early — {progress}"));
        }
        self.goto(Screen::Summary);
    }

    pub fn skip_unit(&mut self, ctx: &egui::Context) {
        if let Some(unit) = self.current_unit() {
            self.note("skip", &format!("{}:{}", unit.file(), unit.start_line()));
        }
        if let Some(plan) = &mut self.plan {
            if plan.advance() {
                self.enter_unit(ctx);
            } else {
                self.goto(Screen::Summary);
            }
        }
    }

    /// Rebuild the current unit from what sits on disk now and start it over.
    /// The snapshot, the context, and any verdicts already collected describe
    /// lines that no longer exist, so the only honest path is a fresh review
    /// of the new text.
    pub fn reload_stale_unit(&mut self, ctx: &egui::Context) {
        let Some(stale) = self.stale_unit.take() else {
            return;
        };
        let Some(repo_path) = self.work_dir() else {
            return;
        };
        if stale.lines.is_empty() {
            // The region is gone from the file entirely — nothing to review.
            self.review_error = None;
            self.note(
                "reload",
                "the unit's lines are gone from disk — skipping it",
            );
            self.skip_unit(ctx);
            return;
        }
        let start_line = stale.start0 as u32 + 1;
        let end_line = stale.start0 as u32 + stale.lines.len() as u32;
        let (file_rel, old_start) = {
            let Some(plan) = &mut self.plan else { return };
            let Some(file) = plan.files.get_mut(plan.file_idx) else {
                return;
            };
            let Some(unit) = file.units.get_mut(plan.unit_idx) else {
                return;
            };
            let context =
                review::disk_context(&repo_path, unit.file(), stale.start0, stale.lines.len(), 6);
            let old_start = unit.start_line();
            // The reloaded numbers describe the file as measured a moment
            // ago; the offsets recorded for the old geometry no longer apply
            // to this unit; saving will locate the unit again if any offset
            // remains.
            match unit {
                ReviewUnit::Comment(u) => {
                    u.indent = stale.lines[0]
                        .chars()
                        .take_while(|c| c.is_whitespace())
                        .collect();
                    u.start_line = start_line;
                    u.end_line = end_line;
                    u.raw_lines = stale.lines;
                    u.context = context;
                }
                ReviewUnit::Code(u) => {
                    u.start_line = start_line;
                    u.end_line = end_line;
                    // Which of these lines the branch added is unknowable
                    // after an outside edit; empty means the change is still
                    // represented but which lines were added is unknown.
                    u.changed_lines.clear();
                    u.raw_lines = stale.lines;
                    u.context = context;
                }
            }
            (unit.file().to_string(), old_start)
        };
        self.note(
            "reload",
            &format!("{file_rel}:{old_start} reloaded from disk as {file_rel}:{start_line}"),
        );
        self.enter_unit(ctx);
    }

    pub fn prev_unit(&mut self, ctx: &egui::Context) {
        if let Some(plan) = &mut self.plan {
            if plan.retreat() {
                self.enter_unit(ctx);
            }
        }
    }

    // -- whole-branch review ---------------------------------------------------------

    /// Ask every enabled model for cross-cutting findings over the whole
    /// branch: what the per-unit review, judging changes in isolation, cannot
    /// see. Runs from the summary screen once the review is done (or whenever
    /// the human asks again).
    pub fn start_whole_branch_review(&mut self, ctx: &egui::Context) {
        let Some(plan) = &self.plan else { return };
        if plan.is_recheck() {
            self.note("branch", "a re-check does not compare an active branch");
            return;
        }
        let Some(repo) = self.work_dir() else {
            return;
        };
        let diff = match gitio::review_diff(&repo, &plan.branch_base, self.settings.context_lines) {
            Ok(d) => d,
            Err(e) => {
                self.note("error", &format!("whole-branch review diff: {e}"));
                return;
            }
        };
        let prompt = findings::build_prompt(
            &plan.ref_name,
            &plan.base_ref,
            plan.files.len(),
            plan.total_units(),
            &diff,
        );
        if let Some(why) = self.usage_block() {
            self.note("usage", &why);
            return;
        }
        let what = format!("{} vs {} · whole branch", plan.ref_name, plan.base_ref);
        self.whole_branch_review_seq += 1;
        self.findings.clear();
        self.whole_branch_review = (0..self.settings.models.len())
            .map(|_| WholeBranchReviewState::Idle)
            .collect();
        let cli_home = self.cli_home(&repo);
        // Reviewing the whole branch takes longer than reviewing one unit.
        let timeout = self.settings.model_timeout_secs.saturating_mul(2);
        let mut launched = 0;
        for (idx, model) in self.settings.enabled_models() {
            // A `{session}` model takes an id of our choosing, exactly as a
            // review turn does. The pass asks nothing further of it, but a
            // reviewer who walks away mid-pass can only pick it up again if
            // the app knows which conversation it was.
            let (command, session) = opening_command(&model);
            let proc = self.procs.register(Owner::Branch, idx, &model.name, &what);
            proc.set_session(session);
            self.whole_branch_review[idx] = WholeBranchReviewState::Pending(proc.clone());
            let tx = self.tx.clone();
            findings::spawn_whole_branch_review(
                self.whole_branch_review_seq,
                idx,
                model,
                command,
                prompt.clone(),
                repo.clone(),
                cli_home.clone(),
                timeout,
                proc,
                move |m| {
                    let _ = tx.send(Msg::WholeBranchReview(m));
                },
                ctx.clone(),
            );
            launched += 1;
        }
        if launched == 0 {
            self.whole_branch_review.clear();
            self.note(
                "branch",
                "no models enabled — nothing to run the whole-branch review with",
            );
            return;
        }
        self.note(
            "branch",
            &format!("whole-branch review started — {launched} model(s) reading the branch"),
        );
    }

    /// Put one model back on the whole-branch review: `resume` continues the
    /// conversation it was holding when it was stopped, and otherwise it is
    /// asked again from a new one.
    ///
    /// Only this model's slice is restarted. The others' findings are already
    /// in and re-running them would charge for answers the screen is showing.
    pub fn rerun_branch_model(&mut self, ctx: &egui::Context, idx: usize, resume: bool) {
        let paused = match self.whole_branch_review.get(idx) {
            Some(WholeBranchReviewState::Paused(p)) => Some(p.clone()),
            _ => None,
        };
        let Some(model_config) = self.settings.models.get(idx).cloned() else {
            return;
        };
        let Some((prompt, repo, what)) = self.branch_prompt() else {
            return;
        };
        if let Some(why) = self.usage_block() {
            self.note("usage", &why);
            return;
        }
        let session = paused.as_ref().and_then(|p| p.session.clone());
        let (command, session) = match (resume, session) {
            (true, Some(id)) => (
                model_config.resume_command.replace("{session}", &id),
                Some(id),
            ),
            _ => opening_command(&model_config),
        };
        let cli_home = self.cli_home(&repo);
        let timeout = self.settings.model_timeout_secs.saturating_mul(2);
        let proc = self
            .procs
            .register(Owner::Branch, idx, &model_config.name, &what);
        proc.set_session(session);
        self.whole_branch_review[idx] = WholeBranchReviewState::Pending(proc.clone());
        let tx = self.tx.clone();
        findings::spawn_whole_branch_review(
            self.whole_branch_review_seq,
            idx,
            model_config.clone(),
            command,
            prompt,
            repo,
            cli_home,
            timeout,
            proc,
            move |m| {
                let _ = tx.send(Msg::WholeBranchReview(m));
            },
            ctx.clone(),
        );
        self.note(
            "branch",
            &format!(
                "{} {} the branch pass",
                model_config.name,
                if resume { "resumed" } else { "restarted" }
            ),
        );
    }

    /// The whole-branch prompt, the repository to run it in, and a label for
    /// the ledger. `None` when the plan cannot support one.
    fn branch_prompt(&mut self) -> Option<(String, String, String)> {
        let plan = self.plan.as_ref()?;
        if plan.is_recheck() {
            return None;
        }
        let repo = self.work_dir()?;
        let base = gitio::base_from_label(&plan.base_ref);
        let diff = match gitio::review_diff(&repo, &base, self.settings.context_lines) {
            Ok(d) => d,
            Err(e) => {
                self.note("error", &format!("whole-branch review diff: {e}"));
                return None;
            }
        };
        let plan = self.plan.as_ref()?;
        let what = format!("{} vs {} · whole branch", plan.ref_name, plan.base_ref);
        let prompt = findings::build_prompt(
            &plan.ref_name,
            &plan.base_ref,
            plan.files.len(),
            plan.total_units(),
            &diff,
        );
        Some((prompt, repo, what))
    }

    pub fn whole_branch_review_running(&self) -> bool {
        self.whole_branch_review
            .iter()
            .any(|s| matches!(s, WholeBranchReviewState::Pending(_)))
    }

    fn handle_whole_branch_review(&mut self, m: WholeBranchReviewMsg) {
        if m.cancelled {
            // Stopped by us; the card is already paused and says so.
            self.note(
                "branch",
                &format!("{} stopped part-way through the branch", m.model),
            );
            return;
        }
        if m.seq != self.whole_branch_review_seq {
            self.db.log(
                "stale",
                &format!("discarded late whole-branch review from {}", m.model),
            );
            return;
        }
        let session_id = self.plan.as_ref().map(|p| p.session_id).unwrap_or(0);
        match m.result {
            Ok(list) => {
                let n = list.len();
                for f in list {
                    let files_json =
                        serde_json::to_string(&f.files).unwrap_or_else(|_| "[]".into());
                    let evidence_json = if f.evidence.is_empty() {
                        None
                    } else {
                        serde_json::to_string(&f.evidence).ok()
                    };
                    let id = self.db.log_finding(
                        session_id,
                        &m.model,
                        f.severity.trim(),
                        &f.title,
                        &f.detail,
                        &files_json,
                        evidence_json.as_deref(),
                    );
                    self.findings.push(FindingRow {
                        id,
                        model: m.model.clone(),
                        finding: f,
                        dismissed: false,
                    });
                }
                // High first; equal severities keep arrival order via the id.
                self.findings
                    .sort_by_key(|r| (r.finding.severity_rank(), r.id));
                if let Some(s) = self.whole_branch_review.get_mut(m.model_index) {
                    *s = WholeBranchReviewState::Done {
                        n,
                        latency_ms: m.latency_ms,
                    };
                }
                self.note(
                    "branch",
                    &format!("{} reported {n} finding(s) ({} ms)", m.model, m.latency_ms),
                );
            }
            Err(e) => {
                if let Some(s) = self.whole_branch_review.get_mut(m.model_index) {
                    *s = WholeBranchReviewState::Failed(e.clone());
                }
                self.note(
                    "branch",
                    &format!(
                        "{} whole-branch review failed: {}",
                        m.model,
                        truncate(&e, 120)
                    ),
                );
            }
        }
    }

    /// Human triage: a dismissed finding stays on the record, marked as such.
    pub fn dismiss_finding(&mut self, id: i64) {
        let mut title = None;
        if let Some(row) = self.findings.iter_mut().find(|r| r.id == id) {
            row.dismissed = true;
            title = Some(truncate(&row.finding.title, 60));
        }
        if let Some(title) = title {
            self.db.set_finding_status(id, "dismissed");
            self.note("branch", &format!("dismissed: {title}"));
        }
    }

    /// The open findings as markdown, for handing to a PR description or an
    /// issue tracker.
    pub fn findings_markdown(&self) -> String {
        let mut out = String::new();
        for row in self.findings.iter().filter(|r| !r.dismissed) {
            let f = &row.finding;
            out.push_str(&format!(
                "- **[{}]** {} _({})_\n  {}\n",
                if f.severity.trim().is_empty() {
                    "?"
                } else {
                    f.severity.trim()
                },
                f.title.trim(),
                row.model,
                f.detail.trim().replace('\n', "\n  ")
            ));
            if !f.files.is_empty() {
                out.push_str(&format!("  files: {}\n", f.files.join(", ")));
            }
        }
        out
    }

    // -- publishing -----------------------------------------------------------

    /// Read where the review's commits stand against the remote, and fill the
    /// stacked pull request's fields in the first time this session's summary
    /// is opened. Run on arriving at the summary screen and again after every
    /// publish, because both routes move the very thing it is describing.
    pub fn refresh_delivery(&mut self) {
        let Some(work) = self.work_dir() else {
            self.delivery = None;
            return;
        };
        let Some(plan) = self.plan.as_ref() else {
            self.delivery = None;
            return;
        };
        // A re-check judges history and writes nothing, so it has nothing to
        // deliver and the section stays off the screen entirely.
        if plan.is_recheck() {
            self.delivery = None;
            return;
        }
        let (ref_name, base, session_id) = (
            plan.ref_name.clone(),
            plan.base_ref.clone(),
            plan.session_id,
        );
        // The session's own commits, so the state describes the review rather
        // than whatever this checkout happens to be sitting on.
        let made = self.db.session_commits(session_id);
        self.delivery = Some(gitio::delivery_state(&work, &ref_name, &base, &made));
        self.fill_stack_form(session_id, &ref_name);
    }

    fn fill_stack_form(&mut self, session_id: i64, ref_name: &str) {
        if self.stack.filled_for == Some(session_id) {
            return;
        }
        let restore = self.restore_blocker().is_none();
        self.stack.base = self.stack_base();
        self.stack.branch = publish::suggested_branch(ref_name);
        self.stack.title = format!("Review fixes for {ref_name}");
        self.stack.body = self.stack_body();
        self.stack.restore = restore;
        self.stack.filled_for = Some(session_id);
    }

    /// The pull request body offered to start from: what the review did, in
    /// the terms the summary screen already reports it in.
    fn stack_body(&self) -> String {
        let Some(p) = &self.plan else {
            return String::new();
        };
        let (decided, committed) = self.db.decision_counts(p.session_id);
        let mut out = format!(
            "Fixes made while reviewing `{}` against `{}`.\n\n- {decided} unit(s) decided, \
             {committed} committed\n",
            p.ref_name, p.base_ref
        );
        let open = self.findings.iter().filter(|r| !r.dismissed).count();
        if open > 0 {
            out.push_str(&format!(
                "- {open} open cross-cutting finding(s) from the whole-branch review\n"
            ));
        }
        out.push_str("\nReviewed-with: code-review-assistant\n");
        out
    }

    /// Why the reviewed branch cannot be put back where the remote has it, or
    /// `None` when it can. Wraps the pure check with this session's commits.
    pub fn restore_blocker(&self) -> Option<String> {
        let state = self.delivery.as_ref()?;
        let plan = self.plan.as_ref()?;
        publish::restore_blocker(
            state,
            &plan.ref_name,
            &self.db.session_commits(plan.session_id),
        )
    }

    /// The branch a stacked pull request should target.
    ///
    /// The reviewed branch itself, when the remote has it — that is what makes
    /// the stack a stack, and puts the fixes in front of whoever owns the
    /// branch. When the remote has never seen it there is nothing to stack on,
    /// so the pull request targets what the review was run against instead.
    pub fn stack_base(&self) -> String {
        let Some(plan) = self.plan.as_ref() else {
            return String::new();
        };
        if self.delivery.as_ref().is_some_and(|d| d.upstream.is_some()) {
            return plan.ref_name.clone();
        }
        let base = plan.base_ref.trim();
        let fallback = || {
            self.repo
                .as_ref()
                .map(|r| r.default_branch.clone())
                .unwrap_or_else(|| "main".into())
        };
        // The pseudo-bases an uncommitted review runs against name no branch,
        // and a remote-tracking base is a branch under another name. The
        // remote is read from the state already in hand rather than from git:
        // this is called on every repaint of the summary screen.
        if base.is_empty() || base.starts_with(':') || base == gitio::EMPTY_TREE {
            return fallback();
        }
        let remote = self
            .delivery
            .as_ref()
            .and_then(|d| d.remote.as_deref())
            .unwrap_or("origin");
        base.strip_prefix(&format!("{remote}/"))
            .unwrap_or(base)
            .to_string()
    }

    /// Push the review's commits onto the branch that was reviewed.
    pub fn start_push(&mut self) {
        self.start_publish(publish::Route::Push);
    }

    /// Put the review's commits on a branch of their own and open a pull
    /// request for them against the reviewed branch.
    pub fn start_stacked_pr(&mut self) {
        let route = publish::Route::Stack(publish::Stack {
            branch: self.stack.branch.trim().to_string(),
            base: self.stack.base.trim().to_string(),
            title: self.stack.title.clone(),
            body: self.stack.body.clone(),
            restore: self.stack.restore,
        });
        self.start_publish(route);
    }

    /// Hand one publish to a worker thread. Pushing and opening a pull request
    /// both talk to the network, which is slow enough that doing it on the UI
    /// thread would freeze the window for the duration.
    fn start_publish(&mut self, route: publish::Route) {
        if self.publish.running() {
            return;
        }
        let (Some(work), Some(plan)) = (self.work_dir(), self.plan.as_ref()) else {
            return;
        };
        let Some(state) = self.delivery.clone() else {
            return;
        };
        let Some(remote) = state.remote.clone() else {
            self.publish = PublishState::Failed(
                "this repository has no remote to publish to — add one with `git remote add`"
                    .to_string(),
            );
            return;
        };
        let req = publish::Request {
            dir: work,
            gh: self.settings.gh_path.clone(),
            remote,
            ref_name: plan.ref_name.clone(),
            session_commits: self.db.session_commits(plan.session_id),
            state,
            route,
        };
        let what = match req.route {
            publish::Route::Push => "pushing",
            publish::Route::Stack(_) => "opening the pull request",
        };
        self.note(
            "publish",
            &format!("{what} — {} commit(s)", req.state.ahead()),
        );
        self.publish = PublishState::Running(what);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Publish(publish::run(&req)));
        });
    }

    fn handle_publish(&mut self, res: Result<publish::Outcome, String>) {
        match res {
            Ok(outcome) => {
                self.note("publish", &outcome.headline);
                for line in &outcome.detail {
                    self.note("publish", line);
                }
                self.publish = PublishState::Done(outcome);
            }
            Err(e) => {
                self.note("error", &format!("publish: {}", truncate(&e, 200)));
                self.publish = PublishState::Failed(e);
            }
        }
        // Both routes moved commits; what the screen says about them has to be
        // re-read rather than left showing the state they were launched from.
        self.refresh_delivery();
    }

    // -- notes & follow-up ----------------------------------------------------

    /// Park the typed note on the current unit. The unit itself still gets
    /// decided here and now — the note is for the issue the unit *revealed*,
    /// which is bigger than the unit's own lines and waits for a fix session
    /// with room to make bigger changes.
    pub fn leave_note(&mut self) {
        let text = self.note_input.trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(unit) = self.current_unit() else {
            return;
        };
        let Some(repo) = self.repo.as_ref().map(|r| r.path.clone()) else {
            return;
        };
        let session_id = self.plan.as_ref().map(|p| p.session_id).unwrap_or(0);
        let excerpt = unit.raw_lines().join("\n");
        self.db.log_note(
            session_id,
            &repo,
            unit.file(),
            unit.start_line(),
            unit.end_line(),
            &excerpt,
            &text,
        );
        self.note_input.clear();
        self.note(
            "note",
            &format!(
                "parked for follow-up: {}:{} — {}",
                unit.file(),
                unit.start_line(),
                truncate(&text, 60)
            ),
        );
    }

    /// Load the backlog and switch to the follow-up screen. Only open notes
    /// come back: resolved and dismissed ones are done being seen.
    pub fn open_followup(&mut self) {
        let Some(repo) = self.repo.as_ref().map(|r| r.path.clone()) else {
            return;
        };
        self.notes = self
            .db
            .open_notes(&repo)
            .into_iter()
            .map(|note| NoteRow {
                note,
                checked: false,
            })
            .collect();
        if self.fix_prompt.trim().is_empty() {
            self.fix_prompt = crate::notes::default_preamble().to_string();
        }
        if !self
            .settings
            .models
            .get(self.selected_fix_model_index)
            .is_some_and(|m| m.enabled)
        {
            self.selected_fix_model_index = self
                .settings
                .enabled_models()
                .first()
                .map(|(i, _)| *i)
                .unwrap_or(0);
        }
        self.fix_error = None;
        if self.screen != Screen::Followup {
            self.followup_from = self.screen;
            self.goto(Screen::Followup);
        }
        self.note("follow-up", &format!("{} open note(s)", self.notes.len()));
    }

    /// Human triage: a dismissed note stays on the record, marked as such,
    /// and is never offered again.
    pub fn dismiss_note(&mut self, id: i64) {
        let Some(pos) = self.notes.iter().position(|r| r.note.id == id) else {
            return;
        };
        let row = self.notes.remove(pos);
        self.db.set_note_status(id, "dismissed");
        self.note(
            "follow-up",
            &format!("dismissed note: {}", truncate(&row.note.text, 60)),
        );
    }

    /// Launch the interactive fix session on the checked notes. Checking is
    /// commitment: the checked notes are marked resolved the moment the
    /// session launches, so the next visit shows only what was left
    /// unchecked — the session's own transcript is where their fate is read.
    pub fn start_fix_session(&mut self, ctx: &egui::Context) {
        let picked: Vec<Note> = self
            .notes
            .iter()
            .filter(|r| r.checked)
            .map(|r| r.note.clone())
            .collect();
        if picked.is_empty() {
            self.fix_error = Some("check at least one note to hand to the session".into());
            return;
        }
        let Some(model_config) = self
            .settings
            .models
            .get(self.selected_fix_model_index)
            .cloned()
        else {
            self.fix_error = Some("no model in that model — configure one in settings".into());
            return;
        };
        if !model_config.enabled {
            self.fix_error = Some(format!("{} is disabled in settings", model_config.name));
            return;
        }
        if model_config.fix_command.trim().is_empty() {
            self.fix_error = Some(format!(
                "{} has no writable fix command — configure one in settings",
                model_config.name
            ));
            return;
        }
        let Some(repo) = self.work_dir() else {
            return;
        };
        if let Some(why) = self.usage_block() {
            self.fix_error = Some(why.clone());
            self.note("usage", &why);
            return;
        }
        let prompt = crate::notes::build_fix_prompt(&self.fix_prompt, &picked);
        self.fix_seq += 1;
        self.active_fix_model_index = self.selected_fix_model_index;
        self.fix_running = true;
        self.fix_error = None;
        self.fix_convo = vec![Turn {
            prompt: prompt.clone(),
            reply: String::new(),
        }];
        self.fix_session = None;
        // Same session setup as a review turn: a model that names no
        // session key takes an id of our choosing, the rest report theirs.
        let command = if model_config.session_key.trim().is_empty()
            && model_config.fix_command.contains("{session}")
        {
            let id = uuid::Uuid::new_v4().to_string();
            let command = model_config.fix_command.replace("{session}", &id);
            self.fix_session = Some(id);
            command
        } else {
            model_config.fix_command.clone()
        };
        let cli_home = self.fix_cli_home(&repo, &command);
        // Resolving a backlog of larger issues is a different order of work
        // from judging one unit; give the session room to do it.
        let timeout = self.settings.model_timeout_secs.saturating_mul(4);
        let what = format!("{} note(s)", picked.len());
        let proc = self.procs.register(
            Owner::Fix,
            self.active_fix_model_index,
            &model_config.name,
            &what,
        );
        proc.set_session(self.fix_session.clone());
        self.fix_proc = Some(proc.clone());
        self.fix_paused = None;
        let tx = self.tx.clone();
        models::spawn_freeform(
            self.fix_seq,
            self.active_fix_model_index,
            model_config.clone(),
            command,
            prompt,
            repo,
            cli_home,
            timeout,
            proc,
            move |m| {
                let _ = tx.send(Msg::Fix(m));
            },
            ctx.clone(),
        );
        for n in &picked {
            self.db.set_note_status(n.id, "resolved");
        }
        self.notes.retain(|r| !r.checked);
        self.note(
            "follow-up",
            &format!(
                "fix session started — {} note(s) handed to {}",
                picked.len(),
                model_config.name
            ),
        );
    }

    /// Whether the fix session can take another message: the previous turn
    /// finished, there is a session to resume, and the model that holds it
    /// knows how.
    pub fn fix_can_resume(&self) -> bool {
        !self.fix_running
            && !self.fix_convo.is_empty()
            && self.fix_session.is_some()
            && self
                .settings
                .models
                .get(self.active_fix_model_index)
                .is_some_and(|m| !m.fix_resume_command.trim().is_empty())
    }

    /// Send the pending message into the live fix session. Free-form on
    /// purpose: this conversation is doing work, not filing verdicts, so no
    /// answer schema rides along.
    pub fn ask_fix_followup(&mut self, ctx: &egui::Context) {
        let message = self.fix_follow_up.trim().to_string();
        if message.is_empty() || !self.fix_can_resume() {
            return;
        }
        if let Some(why) = self.usage_block() {
            self.fix_error = Some(why.clone());
            self.note("usage", &why);
            return;
        }
        let Some(model_config) = self
            .settings
            .models
            .get(self.active_fix_model_index)
            .cloned()
        else {
            return;
        };
        let Some(session) = self.fix_session.clone() else {
            return;
        };
        let Some(repo) = self.work_dir() else {
            return;
        };
        let command = model_config
            .fix_resume_command
            .replace("{session}", &session);
        self.fix_convo.push(Turn {
            prompt: message.clone(),
            reply: String::new(),
        });
        self.fix_seq += 1;
        self.fix_running = true;
        let cli_home = self.fix_cli_home(&repo, &command);
        let timeout = self.settings.model_timeout_secs.saturating_mul(4);
        let proc = self.procs.register(
            Owner::Fix,
            self.active_fix_model_index,
            &model_config.name,
            "follow-up",
        );
        proc.set_session(Some(session));
        self.fix_proc = Some(proc.clone());
        self.fix_paused = None;
        let tx = self.tx.clone();
        models::spawn_freeform(
            self.fix_seq,
            self.active_fix_model_index,
            model_config,
            command,
            message,
            repo,
            cli_home,
            timeout,
            proc,
            move |m| {
                let _ = tx.send(Msg::Fix(m));
            },
            ctx.clone(),
        );
        self.fix_follow_up.clear();
    }

    fn handle_fix(&mut self, m: models::FixMsg) {
        if m.seq != self.fix_seq {
            self.db.log(
                "stale",
                &format!("discarded late fix-session reply from {}", m.model),
            );
            return;
        }
        self.fix_running = false;
        self.fix_proc = None;
        // The reply is read with the launching model's session key, so a moved
        // model picker cannot misread it. This happens before the cancelled
        // check below: a turn killed part-way may still have printed the id of
        // the conversation it was holding, and that id is the only thing that
        // makes resuming it possible.
        if let Some(key) = self
            .settings
            .models
            .get(m.model_index)
            .map(|s| s.session_key.clone())
        {
            if let Some(id) = models::extract_session_id(&m.raw, &key) {
                self.fix_session = Some(id.clone());
                if let Some(p) = self.fix_paused.as_mut().filter(|p| p.session.is_none()) {
                    p.session = Some(id);
                }
            }
        }
        if m.cancelled {
            // What the stopped turn managed to say is what a resume continues
            // from, so it belongs in the transcript like any other reply.
            if let Some(turn) = self.fix_convo.last_mut().filter(|t| t.reply.is_empty()) {
                turn.reply = match m.raw.trim().is_empty() {
                    true => "(stopped before it said anything)".to_string(),
                    false => models::transcript_excerpt(&m.raw),
                };
            }
            self.note(
                "follow-up",
                &format!("{} stopped part-way through its turn", m.model),
            );
            return;
        }
        if let Some(turn) = self.fix_convo.last_mut() {
            turn.reply = if m.raw.trim().is_empty() {
                match &m.result {
                    Ok(_) => "(no output)".to_string(),
                    Err(e) => e.clone(),
                }
            } else {
                models::transcript_excerpt(&m.raw)
            };
        }
        match m.result {
            Ok(latency_ms) => {
                self.note(
                    "follow-up",
                    &format!("{} replied ({latency_ms} ms)", m.model),
                );
            }
            Err(e) => {
                self.fix_error = Some(e.clone());
                self.note(
                    "follow-up",
                    &format!("{} fix session failed: {}", m.model, truncate(&e, 120)),
                );
            }
        }
    }

    // -- async pump ----------------------------------------------------------

    fn pump_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Prs(res) => {
                    self.prs_loading = false;
                    match res {
                        Ok(prs) => {
                            self.note("gh", &format!("{} open PRs", prs.len()));
                            self.prs = prs;
                        }
                        Err(e) => self.prs_error = Some(e),
                    }
                }
                Msg::Repo(m) => self.handle_repo(m),
                Msg::Cand(c) => self.handle_candidate(c),
                Msg::WholeBranchReview(m) => self.handle_whole_branch_review(m),
                Msg::Fix(m) => self.handle_fix(m),
                Msg::Publish(res) => self.handle_publish(res),
            }
        }
    }

    fn handle_candidate(&mut self, c: CandidateMsg) {
        if c.cancelled {
            // This app stopped the call. The card already shows it as paused,
            // and overwriting that with the "failure" our own kill produced
            // would erase the session the reviewer is being offered.
            self.record_stopped_call(&c);
            return;
        }
        if c.seq != self.review_seq {
            if let Some(pos) = self.prefetches.iter().position(|p| p.seq == c.seq) {
                self.handle_prefetch_reply(pos, c);
                return;
            }
            self.db
                .log("stale", &format!("discarded late reply from {}", c.model));
            return;
        }
        let (file, start, end) = self
            .current_unit()
            .map(|u| (u.file().to_string(), u.start_line(), u.end_line()))
            .unwrap_or_default();
        let session_id = self.plan.as_ref().map(|p| p.session_id).unwrap_or(0);
        // Track the CLI's own id so the next turn resumes this conversation.
        // Take the newest one each time: a CLI is free to hand back a fresh id
        // when it resumes, and following it keeps the chain unbroken.
        if let Some(key) = self
            .candidate_models
            .get(c.model_index)
            .map(|m| m.session_key.clone())
        {
            if let Some(id) = models::extract_session_id(&c.raw, &key) {
                if let Some(model) = self.sessions.get_mut(c.model_index) {
                    *model = Some(id);
                }
            }
        }
        if let Some(turn) = self
            .convos
            .get_mut(c.model_index)
            .and_then(|t| t.last_mut())
        {
            // The session id and the verdict have already been pulled out
            // of the full text; what is left is for a human to read.
            turn.reply = reply_text(&c.result, &c.raw);
        }
        // What the call spent, from the CLI's own accounting. Read off the raw
        // output rather than the parsed verdict so a call that produced no
        // verdict is still charged for: a model that burns tokens and then
        // returns nothing is the expensive case, not a free one.
        let usage = models::extract_usage(&c.raw);
        let usage = (!usage.is_silent()).then_some(usage);
        let cost = usage
            .zip(self.candidate_models.get(c.model_index))
            .and_then(|(u, model)| u.priced(model.price_in, model.price_out));
        // The question this answer replies to, if the model had one pending.
        let link = self
            .pending_follow_up
            .get_mut(c.model_index)
            .and_then(|p| p.take());
        let (follow_up_id, round) = match link {
            Some((id, r)) => (Some(id), r),
            None => (None, 1),
        };
        match c.result {
            Ok(s) => {
                let evidence_json = if s.evidence.is_empty() {
                    None
                } else {
                    serde_json::to_string(&s.evidence).ok()
                };
                self.db.log_suggestion(&crate::db::SuggestionRecord {
                    session_id,
                    file: &file,
                    line_start: start,
                    line_end: end,
                    model: &c.model,
                    action: Some(s.action.as_str()),
                    comment: Some(&s.comment),
                    justification: Some(&s.justification),
                    latency_ms: s.latency_ms,
                    error: None,
                    evidence: evidence_json.as_deref(),
                    usage,
                    cost,
                    follow_up_id,
                    round,
                    stopped: false,
                });
                let label = self.model_display(c.model_index);
                if self.names_hidden() {
                    self.note("model", &format!("{label} replied ({} ms)", s.latency_ms));
                } else {
                    let kind = self
                        .current_unit()
                        .map(|u| u.kind())
                        .unwrap_or(units::UnitKind::Comment);
                    self.note(
                        "model",
                        &format!(
                            "{label} → {} ({} ms)",
                            units::action_label(s.action, kind),
                            s.latency_ms
                        ),
                    );
                }
                if let Some(model) = self.candidates.get_mut(c.model_index) {
                    *model = CandidateState::Ready(s);
                }
            }
            Err(e) => {
                self.db.log_suggestion(&crate::db::SuggestionRecord {
                    session_id,
                    file: &file,
                    line_start: start,
                    line_end: end,
                    model: &c.model,
                    action: None,
                    comment: None,
                    justification: None,
                    latency_ms: 0,
                    error: Some(&e),
                    evidence: None,
                    usage,
                    cost,
                    follow_up_id,
                    round,
                    stopped: false,
                });
                let label = self.model_display(c.model_index);
                self.note("model", &format!("{label} failed: {}", truncate(&e, 120)));
                if let Some(model) = self.candidates.get_mut(c.model_index) {
                    *model = CandidateState::Failed(e);
                }
            }
        }
    }

    /// An early reply for a unit the review has not reached yet: record it —
    /// the call was spent whether or not the unit is ever entered — and bank
    /// it for adoption. The activity line stays neutral about which model
    /// answered; while blinded, even that much is a tell.
    fn handle_prefetch_reply(&mut self, pos: usize, c: CandidateMsg) {
        let usage = models::extract_usage(&c.raw);
        let usage = (!usage.is_silent()).then_some(usage);
        let cost = usage
            .zip(self.prefetches[pos].models.get(c.model_index))
            .and_then(|(u, model)| u.priced(model.price_in, model.price_out));
        let (session_id, file, start, end) = {
            let pf = &self.prefetches[pos];
            (pf.session_id, pf.file.clone(), pf.start_line, pf.end_line)
        };
        match &c.result {
            Ok(s) => {
                let evidence_json = if s.evidence.is_empty() {
                    None
                } else {
                    serde_json::to_string(&s.evidence).ok()
                };
                self.db.log_suggestion(&crate::db::SuggestionRecord {
                    session_id,
                    file: &file,
                    line_start: start,
                    line_end: end,
                    model: &c.model,
                    action: Some(s.action.as_str()),
                    comment: Some(&s.comment),
                    justification: Some(&s.justification),
                    latency_ms: s.latency_ms,
                    error: None,
                    evidence: evidence_json.as_deref(),
                    usage,
                    cost,
                    follow_up_id: None,
                    round: 1,
                    stopped: false,
                });
                self.note("model", &format!("early answer in for {file}:{start}"));
            }
            Err(e) => {
                self.db.log_suggestion(&crate::db::SuggestionRecord {
                    session_id,
                    file: &file,
                    line_start: start,
                    line_end: end,
                    model: &c.model,
                    action: None,
                    comment: None,
                    justification: None,
                    latency_ms: 0,
                    error: Some(e),
                    evidence: None,
                    usage,
                    cost,
                    follow_up_id: None,
                    round: 1,
                    stopped: false,
                });
                self.note(
                    "model",
                    &format!(
                        "early answer for {file}:{start} failed: {}",
                        truncate(e, 120)
                    ),
                );
            }
        }
        if let Some(r) = self.prefetches[pos].replies.get_mut(c.model_index) {
            *r = Some(PrefetchedReply {
                result: c.result,
                raw: c.raw,
            });
        }
    }

    // -- processes, sessions and navigation ---------------------------------

    /// Move to another screen, stopping whatever the screen being left had
    /// running.
    ///
    /// Every navigation goes through here, which is the point: a CLI call
    /// outlives the page that started it only because some path forgot to end
    /// it, and there is no reliable way to remember at eight call sites. What
    /// was stopped is recorded in [`CraApp::nav_notice`] so the reviewer sees
    /// it happen rather than having to trust that it did.
    pub fn goto(&mut self, to: Screen) {
        if self.screen == to {
            return;
        }
        self.leave_screen();
        self.screen = to;
        if to == Screen::Summary {
            // Reading it here, not per frame: it is a handful of git commands,
            // and the summary repaints on every pointer move.
            self.refresh_delivery();
        }
    }

    /// Stop the current screen's model calls and turn them into paused ones.
    fn leave_screen(&mut self) {
        let from = self.screen;
        let Some(owner) = from.owns() else { return };
        let reason = format!("left the {}", from.label());
        let receipts = self.procs.stop(owner, &reason);
        let (paused, resumable) = match owner {
            Owner::Review => self.pause_review(&reason),
            Owner::Branch => self.pause_branch(&reason),
            Owner::Fix => self.pause_fix(&reason),
        };
        if receipts.is_empty() && paused == 0 {
            // Nothing was running. Saying so would be noise, and a stale
            // banner about an earlier departure would be worse than noise.
            self.nav_notice = None;
            return;
        }
        self.note(
            "procs",
            &format!(
                "{reason} — {} process(es) being terminated, {paused} session(s) paused ({resumable} resumable)"
            ,   receipts.len()),
        );
        self.nav_notice = Some(NavNotice {
            left: from,
            receipts,
            resumable,
        });
    }

    /// Turn every running per-unit call into a paused one.
    ///
    /// Returns how many were paused and how many of those can be continued on
    /// the same session. The two differ whenever a CLI reports its session id
    /// only in its reply: killed before it answered, it leaves nothing to
    /// resume, and the card has to offer a fresh ask instead of pretending.
    fn pause_review(&mut self, reason: &str) -> (usize, usize) {
        let mut paused = 0;
        let mut resumable = 0;
        for idx in 0..self.candidates.len() {
            let handle = match &self.candidates[idx] {
                CandidateState::Pending(h) => h.clone(),
                _ => continue,
            };
            let fallback = self.sessions.get(idx).cloned().flatten();
            let call = self.paused_call(
                Owner::Review,
                idx,
                handle,
                reason,
                self.convo_prompt(idx),
                fallback,
            );
            if call.resumable(&self.settings) {
                resumable += 1;
            }
            paused += 1;
            self.candidates[idx] = CandidateState::Paused(call);
        }
        // The prefetch is the review's work too, and the only work with no
        // card of its own — which makes it the easiest call to lose. Its
        // processes have just been stopped, so an unfinished one is dropped
        // rather than left looking answerable; a complete one cost nothing
        // more to keep and its answers are already paid for.
        self.prefetches.retain(|p| p.complete());
        (paused, resumable)
    }

    fn pause_branch(&mut self, reason: &str) -> (usize, usize) {
        let mut paused = 0;
        let mut resumable = 0;
        for idx in 0..self.whole_branch_review.len() {
            let handle = match &self.whole_branch_review[idx] {
                WholeBranchReviewState::Pending(h) => h.clone(),
                _ => continue,
            };
            let call = self.paused_call(Owner::Branch, idx, handle, reason, String::new(), None);
            if call.resumable(&self.settings) {
                resumable += 1;
            }
            paused += 1;
            self.whole_branch_review[idx] = WholeBranchReviewState::Paused(call);
        }
        (paused, resumable)
    }

    fn pause_fix(&mut self, reason: &str) -> (usize, usize) {
        let Some(handle) = self.fix_proc.take() else {
            return (0, 0);
        };
        let idx = self.active_fix_model_index;
        let prompt = self
            .fix_convo
            .last()
            .map(|t| t.prompt.clone())
            .unwrap_or_default();
        let call = self.paused_call(
            Owner::Fix,
            idx,
            handle,
            reason,
            prompt,
            self.fix_session.clone(),
        );
        let resumable = usize::from(call.resumable(&self.settings));
        // The session id the app holds is the fix screen's own record of the
        // conversation, and it outlasts any one turn.
        self.fix_session = call.session.clone().or_else(|| self.fix_session.clone());
        self.fix_running = false;
        self.fix_paused = Some(call);
        (1, resumable)
    }

    /// One stopped call, as the screen that owned it will show it.
    fn paused_call(
        &self,
        owner: Owner,
        model_index: usize,
        handle: ProcHandle,
        reason: &str,
        prompt: String,
        fallback_session: Option<String>,
    ) -> PausedCall {
        let snap = handle.snapshot();
        let model = self
            .settings
            .models
            .get(model_index)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| "model".to_string());
        // Prefer what the process itself reported; fall back to the id the app
        // generated for it, which for a `{session}` model is the same id and
        // is known even when the call was killed before it printed anything.
        let session = snap.session.clone().or(fallback_session);
        // By handle rather than by pid: a call stopped before it started has
        // no pid, and matching on `None` would pick some other row's label.
        let what = self
            .procs
            .row_for(&handle)
            .map(|r| r.what.clone())
            .unwrap_or_default();
        PausedCall {
            owner,
            model_index,
            model,
            what,
            pid: snap.pid,
            session,
            ran_for: snap.elapsed,
            usage: snap.usage,
            prompt,
            reason: reason.to_string(),
        }
    }

    /// The prompt a model was last sent for the current unit.
    fn convo_prompt(&self, model_index: usize) -> String {
        self.convos
            .get(model_index)
            .and_then(|c| c.last())
            .map(|t| t.prompt.clone())
            .unwrap_or_default()
    }

    /// Stop everything, everywhere — the quit path, and the "stop all" button
    /// on the process ledger.
    pub fn stop_all_models(&mut self, reason: &str) -> usize {
        let receipts = self.procs.stop_all(reason);
        let n = receipts.len();
        for owner in Owner::ALL {
            match owner {
                Owner::Review => self.pause_review(reason),
                Owner::Branch => self.pause_branch(reason),
                Owner::Fix => self.pause_fix(reason),
            };
        }
        if n > 0 {
            self.note("procs", &format!("{reason} — stopping {n} process(es)"));
            self.nav_notice = Some(NavNotice {
                left: self.screen,
                receipts,
                resumable: 0,
            });
        }
        n
    }

    /// Stop one ledger row and immediately mirror that stop into the owning
    /// screen's state. Otherwise the process dies while its card keeps saying
    /// "running" forever because cancelled replies deliberately do not
    /// overwrite UI state.
    pub fn stop_model(&mut self, id: u64, reason: &str) -> usize {
        let receipts = self.procs.stop_one(id, reason);
        let mut resumable = 0;
        for receipt in &receipts {
            resumable += self.pause_one(receipt, reason);
        }
        let n = receipts.len();
        if n > 0 {
            self.note(
                "procs",
                &format!("stopping {n} process(es) from the ledger"),
            );
            self.nav_notice = Some(NavNotice {
                left: self.screen,
                receipts,
                resumable,
            });
        }
        n
    }

    fn pause_one(&mut self, receipt: &StopReceipt, reason: &str) -> usize {
        let idx = receipt.model_index;
        match receipt.owner {
            Owner::Review => {
                let pending = self.candidates.get(idx).and_then(|state| match state {
                    CandidateState::Pending(handle) if handle.is(&receipt.handle) => {
                        Some(handle.clone())
                    }
                    _ => None,
                });
                if let Some(handle) = pending {
                    let fallback = self.sessions.get(idx).cloned().flatten();
                    let call = self.paused_call(
                        Owner::Review,
                        idx,
                        handle,
                        reason,
                        self.convo_prompt(idx),
                        fallback,
                    );
                    let resumable = usize::from(call.resumable(&self.settings));
                    self.candidates[idx] = CandidateState::Paused(call);
                    resumable
                } else {
                    // A prefetched call has no visible card. Drop its unfinished
                    // prefetch so it can be asked cleanly when reached.
                    self.prefetches.retain(|pf| {
                        !pf.lives.iter().flatten().any(|h| h.is(&receipt.handle)) || pf.complete()
                    });
                    0
                }
            }
            Owner::Branch => {
                let pending = self
                    .whole_branch_review
                    .get(idx)
                    .and_then(|state| match state {
                        WholeBranchReviewState::Pending(handle) if handle.is(&receipt.handle) => {
                            Some(handle.clone())
                        }
                        _ => None,
                    });
                if let Some(handle) = pending {
                    let call =
                        self.paused_call(Owner::Branch, idx, handle, reason, String::new(), None);
                    let resumable = usize::from(call.resumable(&self.settings));
                    self.whole_branch_review[idx] = WholeBranchReviewState::Paused(call);
                    resumable
                } else {
                    0
                }
            }
            Owner::Fix => {
                let pending = self
                    .fix_proc
                    .as_ref()
                    .filter(|handle| handle.is(&receipt.handle))
                    .cloned();
                if let Some(handle) = pending {
                    let prompt = self
                        .fix_convo
                        .last()
                        .map(|turn| turn.prompt.clone())
                        .unwrap_or_default();
                    let call = self.paused_call(
                        Owner::Fix,
                        idx,
                        handle,
                        reason,
                        prompt,
                        self.fix_session.clone(),
                    );
                    self.fix_session = call.session.clone();
                    self.fix_running = false;
                    self.fix_proc = None;
                    let resumable = usize::from(call.resumable(&self.settings));
                    self.fix_paused = Some(call);
                    resumable
                } else {
                    0
                }
            }
        }
    }

    /// Continue a paused per-unit call on the session it was holding.
    pub fn resume_candidate(&mut self, ctx: &egui::Context, idx: usize) {
        let CandidateState::Paused(call) = &self.candidates[idx] else {
            return;
        };
        let call = call.clone();
        let Some(model_config) = self.candidate_models.get(idx).cloned() else {
            return;
        };
        let Some(session) = call.session.clone() else {
            self.review_error = Some(format!(
                "{} left no session id behind — ask it again instead",
                call.model
            ));
            return;
        };
        if let Some(why) = self.usage_block() {
            self.review_error = Some(why);
            return;
        }
        let command = model_config.resume_command.replace("{session}", &session);
        let prompt = match call.prompt.trim().is_empty() {
            true => match self.current_unit() {
                Some(u) => self.unit_prompt(&u),
                None => return,
            },
            false => call.prompt.clone(),
        };
        self.sessions[idx] = Some(session.clone());
        self.spawn_one(
            ctx,
            idx,
            model_config,
            command,
            prompt,
            Some(session),
            &call.what,
        );
        // Neither the name nor the session id while blinded: the id is printed
        // beside the real name in the process ledger.
        let label = self.model_display(idx);
        self.note("procs", &format!("resumed {label} on its open session"));
    }

    /// Ask a model the current unit again from a new session — what is on
    /// offer when a killed call left no session id to continue.
    pub fn restart_candidate(&mut self, ctx: &egui::Context, idx: usize) {
        let Some(model_config) = self.candidate_models.get(idx).cloned() else {
            return;
        };
        let Some(unit) = self.current_unit() else {
            return;
        };
        if let Some(why) = self.usage_block() {
            self.review_error = Some(why);
            return;
        }
        let (command, session) = opening_command(&model_config);
        self.sessions[idx] = session.clone();
        let prompt = self.unit_prompt(&unit);
        let what = format!("{}:{} · verdict", unit.file(), unit.start_line());
        self.spawn_one(ctx, idx, model_config, command, prompt, session, &what);
        let label = self.model_display(idx);
        self.note("procs", &format!("restarted {label} on a new session"));
    }

    /// Start one model on the current unit, replacing whatever card it had.
    /// Shared by resuming and restarting, which differ only in the command
    /// they build and the session they carry.
    #[allow(clippy::too_many_arguments)]
    fn spawn_one(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        model_config: crate::settings::ModelConfig,
        command: String,
        prompt: String,
        session: Option<String>,
        what: &str,
    ) {
        let repo_path = self.work_dir_or_default();
        let cli_home = self.cli_home(&repo_path);
        let timeout = self.settings.model_timeout_secs;
        if let Some(convo) = self.convos.get_mut(idx) {
            convo.push(Turn {
                prompt: prompt.clone(),
                reply: String::new(),
            });
        }
        let proc = self
            .procs
            .register(Owner::Review, idx, &model_config.name, what);
        proc.set_session(session);
        self.candidates[idx] = CandidateState::Pending(proc.clone());
        self.review_error = None;
        let tx = self.tx.clone();
        models::spawn_model(
            self.review_seq,
            idx,
            model_config,
            command,
            prompt,
            repo_path,
            cli_home,
            timeout,
            proc,
            move |m| {
                let _ = tx.send(Msg::Cand(m));
            },
            ctx.clone(),
        );
    }

    /// Continue a paused fix-session turn on the session it was holding.
    pub fn resume_fix(&mut self, ctx: &egui::Context) {
        let Some(call) = self.fix_paused.clone() else {
            return;
        };
        let Some(model_config) = self.settings.models.get(call.model_index).cloned() else {
            return;
        };
        let Some(session) = call.session.clone().or_else(|| self.fix_session.clone()) else {
            self.fix_error =
                Some("that turn left no session id behind — start a new session instead".into());
            return;
        };
        let Some(repo) = self.work_dir() else { return };
        if let Some(why) = self.usage_block() {
            self.fix_error = Some(why);
            return;
        }
        let command = model_config
            .fix_resume_command
            .replace("{session}", &session);
        // The same instructions again, on the conversation that already has
        // them: the model picks up where the kill interrupted it rather than
        // being handed a summary of what it was doing.
        let prompt = match call.prompt.trim().is_empty() {
            true => "Continue the work you were part-way through.".to_string(),
            false => call.prompt.clone(),
        };
        self.fix_seq += 1;
        self.fix_running = true;
        self.fix_error = None;
        self.fix_session = Some(session.clone());
        self.active_fix_model_index = call.model_index;
        let cli_home = self.fix_cli_home(&repo, &command);
        let timeout = self.settings.model_timeout_secs.saturating_mul(4);
        let proc = self.procs.register(
            Owner::Fix,
            call.model_index,
            &model_config.name,
            "resumed turn",
        );
        proc.set_session(Some(session.clone()));
        self.fix_proc = Some(proc.clone());
        self.fix_paused = None;
        let tx = self.tx.clone();
        models::spawn_freeform(
            self.fix_seq,
            call.model_index,
            model_config,
            command,
            prompt,
            repo,
            cli_home,
            timeout,
            proc,
            move |m| {
                let _ = tx.send(Msg::Fix(m));
            },
            ctx.clone(),
        );
        self.note("procs", &format!("resumed the fix session on {session}"));
    }

    /// The unit the review screen still holds, if any — what a "back to the
    /// review" affordance offers to return to. Returning is not the same as
    /// picking a file: it puts the reviewer back on the unit they left, with
    /// its answers and its paused calls where they were.
    pub fn review_in_progress(&self) -> Option<String> {
        let (file, line, _) = self.reviewing.as_ref()?;
        self.plan.as_ref()?;
        Some(format!("{file}:{line}"))
    }

    /// Whether anything on the current screen is waiting to be picked up.
    pub fn paused_here(&self) -> usize {
        match self.screen {
            Screen::Review => self
                .candidates
                .iter()
                .filter(|c| matches!(c, CandidateState::Paused(_)))
                .count(),
            Screen::Summary => self
                .whole_branch_review
                .iter()
                .filter(|s| matches!(s, WholeBranchReviewState::Paused(_)))
                .count(),
            Screen::Followup => usize::from(self.fix_paused.is_some()),
            _ => 0,
        }
    }

    /// Why no new model call may start, if anything says so.
    ///
    /// The ceiling is measured over this run of the app, against what the CLIs
    /// themselves reported spending. A model whose CLI reports nothing cannot
    /// be counted, and is deliberately not guessed at: a limit that stopped
    /// work on an estimate would stop it for the wrong reason.
    pub fn usage_block(&self) -> Option<String> {
        let spent = self.procs.spent();
        let limit_usd = self.settings.usage_limit_usd;
        if limit_usd > 0.0 {
            if let Some(usd) = spent.cost_usd.filter(|u| *u >= limit_usd) {
                return Some(format!(
                    "usage limit reached — ${usd:.2} spent this run of ${limit_usd:.2}. \
                     Raise or clear the limit in settings to keep going."
                ));
            }
        }
        let limit_tokens = self.settings.usage_limit_tokens;
        if limit_tokens > 0 && spent.tokens() >= limit_tokens {
            return Some(format!(
                "usage limit reached — {} tokens this run of {limit_tokens}. \
                 Raise or clear the limit in settings to keep going.",
                spent.tokens()
            ));
        }
        None
    }

    /// How close this run is to its limit, as a fraction, when one is set.
    pub fn usage_fraction(&self) -> Option<f64> {
        let spent = self.procs.spent();
        let by_cost = (self.settings.usage_limit_usd > 0.0)
            .then(|| spent.cost_usd.unwrap_or(0.0) / self.settings.usage_limit_usd);
        let by_tokens = (self.settings.usage_limit_tokens > 0)
            .then(|| spent.tokens() as f64 / self.settings.usage_limit_tokens as f64);
        match (by_cost, by_tokens) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// Write a finished conversation to the database.
    ///
    /// The CLI goes on holding the conversation after this app closes — that
    /// is what a session is — so the id has to outlive the run that opened it
    /// or the conversation becomes unreachable: still there, still costing
    /// what it cost, and no longer nameable by anything here. A call that
    /// never reported an id has nothing to write down, and writing a row
    /// without one would only pretend otherwise.
    fn record_session(&mut self, done: &crate::procs::Completed) {
        let Some(session) = done.session.clone() else {
            return;
        };
        let repo = self
            .repo
            .as_ref()
            .map(|r| r.path.clone())
            .unwrap_or_default();
        let review = self.plan.as_ref().map(|p| p.session_id);
        let state = if done.ended.interrupted() {
            "paused"
        } else {
            "finished"
        };
        self.db.record_cli_session(&crate::db::CliSessionRecord {
            session: &session,
            review,
            owner: done.owner.label(),
            model: &done.model,
            repo: &repo,
            what: &done.what,
            state,
            pid: done.pid,
            usage: (!done.usage.is_silent()).then_some(done.usage),
        });
    }

    /// Conversations an earlier run of the app left paused in this repository.
    /// Shown in the ledger: they are still real, and this app is the only
    /// thing that wrote their ids down.
    pub fn earlier_paused_sessions(&self) -> Vec<crate::db::PausedSessionRow> {
        match self.repo.as_ref() {
            Some(r) => self.db.paused_cli_sessions(&r.path),
            None => Vec::new(),
        }
    }

    /// Record a call this app stopped. Its spend is real and stays on the
    /// books; its silence is not the model's fault, so the row is marked and
    /// the evaluation page never scores a model for a call the reviewer cut
    /// short.
    fn record_stopped_call(&mut self, c: &CandidateMsg) {
        // The locus only when it is known: the reply of a prefetch dropped on
        // the way out belongs to a unit the review never reached, and naming
        // the current one instead would be a wrong claim rather than a missing
        // one. Nothing reads the locus of a stopped row — its spend counts per
        // model — so an empty one costs nothing and asserts nothing.
        let (file, start, end) = match self.prefetches.iter().find(|p| p.seq == c.seq) {
            Some(pf) => (pf.file.clone(), pf.start_line, pf.end_line),
            None if c.seq == self.review_seq => self
                .current_unit()
                .map(|u| (u.file().to_string(), u.start_line(), u.end_line()))
                .unwrap_or_default(),
            None => (String::new(), 0, 0),
        };
        let session_id = self.plan.as_ref().map(|p| p.session_id).unwrap_or(0);
        let usage = models::extract_usage(&c.raw);
        let usage = (!usage.is_silent()).then_some(usage);
        let cost = usage
            .zip(self.settings.models.get(c.model_index))
            .and_then(|(u, model)| u.priced(model.price_in, model.price_out));
        let reason = match &c.result {
            Err(e) => e.clone(),
            Ok(_) => "stopped".to_string(),
        };
        self.db.log_suggestion(&crate::db::SuggestionRecord {
            session_id,
            file: &file,
            line_start: start,
            line_end: end,
            model: &c.model,
            action: None,
            comment: None,
            justification: None,
            latency_ms: 0,
            error: Some(&reason),
            evidence: None,
            usage,
            cost,
            follow_up_id: None,
            round: self.unit_round,
            stopped: true,
        });
        // Named by display position while blinded, like every other line
        // this log carries about a model.
        let label = self.model_display(c.model_index);
        self.note("procs", &format!("{label} {reason}"));
    }

    pub fn open_settings(&mut self) {
        if self.screen != Screen::Settings {
            self.prev_screen = self.screen;
            self.goto(Screen::Settings);
        }
    }

    pub fn close_settings(&mut self) {
        self.settings.save(&self.db);
        self.note("settings", "saved");
        self.goto(self.prev_screen);
    }
}

/// The opening command for a model. A model that names no session key takes an
/// id of our choosing, returned so the caller can remember it; the rest
/// report theirs in the reply and it is picked up there.
fn opening_command(model_config: &crate::settings::ModelConfig) -> (String, Option<String>) {
    if model_config.session_key.trim().is_empty() && model_config.command.contains("{session}") {
        let id = uuid::Uuid::new_v4().to_string();
        (model_config.command.replace("{session}", &id), Some(id))
    } else {
        (model_config.command.clone(), None)
    }
}

/// The transcript-worthy text of a reply: the CLI's own words when it printed
/// any, otherwise the error or a placeholder.
fn reply_text(result: &Result<Suggestion, String>, raw: &str) -> String {
    if raw.trim().is_empty() {
        match result {
            Ok(_) => "(no output)".to_string(),
            Err(e) => e.clone(),
        }
    } else {
        models::transcript_excerpt(raw)
    }
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    }
}

impl eframe::App for CraApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_messages();
        // Fold finished calls into the ledger before anything reads it, so the
        // running count, the spend and the limit are all one frame's truth.
        for done in self.procs.sweep() {
            self.record_session(&done);
        }
        self.handle_close(ctx);
        if self.procs.running_total() > 0
            || self
                .candidates
                .iter()
                .any(|c| matches!(c, CandidateState::Pending(_)))
            || self.prs_loading
            || self.scanning_local
            || self.scanning_gh
            || self.cloning.is_some()
            || self.whole_branch_review_running()
            || self.fix_running
            || self.publish.running()
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }

        self.global_hotkeys(ctx);
        crate::ui::chrome::top_bar(self, ctx);
        // Directly under the breadcrumb, above everything a screen draws: what
        // leaving the last page stopped is the first thing to read on landing.
        crate::ui::procs_panel::nav_notice(self, ctx);
        crate::ui::chrome::hotkey_bar(self, ctx);
        if self.screen != Screen::RepoPicker && self.screen != Screen::Settings {
            crate::ui::chrome::side_panel(self, ctx);
        }

        egui::CentralPanel::default().show(ctx, |ui| match self.screen {
            Screen::RepoPicker => self.ui_repo_picker(ctx, ui),
            Screen::RefPicker => self.ui_ref_picker(ctx, ui),
            Screen::FilePicker => self.ui_file_picker(ctx, ui),
            Screen::Review => self.ui_review(ctx, ui),
            Screen::Summary => self.ui_summary(ctx, ui),
            Screen::Followup => self.ui_followup(ctx, ui),
            Screen::Eval => self.ui_eval(ctx, ui),
            Screen::Settings => self.ui_settings(ctx, ui),
        });
        crate::ui::procs_panel::window(self, ctx);
    }
}

impl CraApp {
    /// Hold the window open until the model CLIs are actually dead.
    ///
    /// Closing is leaving every page at once, and the one departure with no
    /// window left to report an orphan in. A stop is only a request until a
    /// worker thread acts on it, and those threads go when the process does —
    /// so the close is cancelled once, the kills are asked for, and the real
    /// close waits for every child to confirm.
    pub(crate) fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
            let n = self.stop_all_models("the app is closing");
            if n > 0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.closing = true;
                self.note("procs", &format!("closing — terminating {n} process(es)"));
            }
        }
        if self.closing {
            if self.procs.running_total() == 0 {
                self.note("procs", "closing — every process terminated");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
    }

    pub(crate) fn global_hotkeys(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers};
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Q)) {
            // Asking for the close rather than doing anything about the models
            // here: the frame loop's close handler is the one place that waits
            // for them to actually die, and quitting must go through it like
            // the window's own close button does.
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // Ctrl+E from anywhere, and Esc returns to whatever was on screen. The
        // page itself reads history and changes nothing — but going to it is
        // still leaving, so it stops the models the last page had running and
        // says so, like any other departure. Coming back offers them.
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::E)) {
            if self.screen == Screen::Eval {
                self.goto(self.prev_screen);
            } else {
                self.open_eval();
            }
        }
        // The ledger is a window, not a screen: opening it interrupts
        // nothing, so it needs no navigation and stops no processes.
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::P)) {
            self.show_procs = !self.show_procs;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Comma)) {
            if self.screen == Screen::Settings {
                self.close_settings();
            } else {
                self.open_settings();
            }
        }
        let typing = ctx.wants_keyboard_input();
        if !typing && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            match self.screen {
                Screen::Settings => self.close_settings(),
                Screen::Review => self.goto(Screen::FilePicker),
                Screen::FilePicker => self.goto(Screen::RefPicker),
                Screen::RefPicker | Screen::Summary => self.goto(Screen::RepoPicker),
                Screen::Followup => self.goto(self.followup_from),
                Screen::Eval => self.goto(self.prev_screen),
                Screen::RepoPicker => {}
            }
        }
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;
    use crate::testkit::{FakeCli, FakeCliSpec, TempDir, TempRepo};

    const VERDICT: &str =
        "{\"action\":\"rewrite\",\"comment\":\"Counts retries.\",\"justification\":\"says why\"}";

    /// Two consecutive redundant comments, so tests can watch a second edit
    /// be applied after the first has already shifted the line numbers.
    const LIB_RS: &str = concat!(
        "fn main() {\n",
        "    // Increment the counter by one\n",
        "    counter += 1;\n",
        "    // Reset the counter to zero\n",
        "    counter = 0;\n",
        "}\n",
    );

    struct Harness {
        app: CraApp,
        repo: TempRepo,
        _dir: TempDir,
    }

    impl Harness {
        /// An app with its own database and a real repository whose feature
        /// branch adds two reviewable comments.
        fn new(tag: &str) -> Harness {
            let dir = TempDir::new(tag);
            let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
            let mut app = CraApp::with_db(db);
            app.settings.models.clear();

            let repo = TempRepo::new(tag);
            repo.write("src/lib.rs", "fn main() {}\n");
            repo.commit("base");
            repo.git(&["checkout", "-b", "feature"]);
            repo.write("src/lib.rs", LIB_RS);
            repo.commit("add counter");

            app.repo = Some(RepoCtx {
                path: repo.path(),
                name: "test-repo".into(),
                default_branch: "main".into(),
            });
            let session_id = app
                .db
                .new_session(&repo.path(), "branch", "feature", "main");
            app.plan = Some(Self::plan(&repo, session_id));
            Harness {
                app,
                repo,
                _dir: dir,
            }
        }

        fn plan(repo: &TempRepo, session_id: i64) -> ReviewPlan {
            let diff = gitio::review_diff(&repo.path(), "main", 12).expect("diff");
            let files = crate::diffparse::parse(&diff);
            let extracted = crate::comments::extract_units(&files, 12);
            assert_eq!(extracted.len(), 1, "expected one reviewable file");
            let files = extracted
                .into_iter()
                .map(|(path, units)| {
                    ReviewFile::new(path, units.into_iter().map(ReviewUnit::Comment).collect())
                })
                .collect();
            ReviewPlan {
                session_id,
                ref_kind: RefKind::Branch,
                ref_name: "feature".into(),
                base_ref: "main".into(),
                branch_base: "main".into(),
                files,
                file_idx: 0,
                unit_idx: 0,
                decided_total: 0,
                skipped_decided: 0,
            }
        }

        /// Point every model at a fake CLI and start the review, through the
        /// same entry point the file picker uses.
        fn enter_with(&mut self, model_configs: Vec<crate::settings::ModelConfig>) {
            self.app.settings.models = model_configs;
            self.app.start_review(&egui::Context::default(), 0);
        }

        /// Wait until every model has replied, or give up.
        fn wait_for_model_replies(&mut self) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                self.app.pump_messages();
                let pending = self
                    .app
                    .candidates
                    .iter()
                    .any(|c| matches!(c, CandidateState::Pending(_)));
                if !pending {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "models never came back"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        /// Wait until every prefetch has all its answers, or give up.
        fn wait_for_prefetch_replies(&mut self) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                self.app.pump_messages();
                if self.app.prefetches.iter().all(|p| p.complete()) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "prefetch never came back"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    /// A helper for the lifecycle tests: a model whose CLI takes long enough
    /// to still be running when the reviewer walks away, and whose session id
    /// this app generates — so an interrupted call has something to resume.
    fn slow_resumable(dir: &TempDir, tag: &str) -> (FakeCli, crate::settings::ModelConfig) {
        let cli = FakeCli::new(
            dir,
            tag,
            FakeCliSpec {
                reply: VERDICT,
                delay_secs: 30,
                ..Default::default()
            },
        );
        let mut model_config = cli.model_config("--session-id {session}");
        model_config.resume_command = format!("{} --resume {{session}}", cli.command());
        (cli, model_config)
    }

    /// Wait until every process the ledger knows about has confirmed it is
    /// gone. This is what the banner's tick waits for too.
    fn wait_until_stopped(app: &mut CraApp) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            app.pump_messages();
            // The same order the frame loop uses: sweeping is what hands a
            // finished conversation over to be written down, so a test that
            // swept separately would lose it.
            for done in app.procs.sweep() {
                app.record_session(&done);
            }
            if app.procs.running_total() == 0 {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a process never confirmed the kill"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Wait for the fake CLI to record a command line containing `needle`.
    /// Reading argv rather than waiting for a reply lets a test assert what a
    /// long-running call was launched with without sitting out its whole run.
    fn wait_for_argv(cli: &FakeCli, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let argv = cli.argv_seen();
            if argv.contains(needle) {
                return argv;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the CLI was never run with {needle:?}; last saw {argv:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Wait until a call is actually running, so a test stops a process rather
    /// than a plan to start one.
    fn wait_until_running(app: &mut CraApp) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if app.procs.live().any(|r| r.snapshot().pid.is_some()) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no process ever started"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Walking off the review screen has to end the CLIs it started. Before
    /// this, they ran to completion against a unit nobody would ever see the
    /// verdict for — spending the whole time, and holding sessions the app had
    /// stopped tracking.
    #[test]
    fn leaving_the_review_screen_terminates_its_processes_and_pauses_the_sessions() {
        let dir = TempDir::new("leave_review");
        let (_cli, model_config) = slow_resumable(&dir, "slow");
        let mut h = Harness::new("leave_review");
        h.app.settings.prefetch_next = false;
        h.enter_with(vec![model_config]);
        wait_until_running(&mut h.app);

        let pid = h.app.procs.live().next().and_then(|r| r.snapshot().pid);
        assert!(pid.is_some(), "the call should be a real process by now");

        h.app.goto(Screen::FilePicker);

        // The reviewer is told what happened, by pid, and the claim is only
        // "terminated" once the process itself has confirmed it.
        let notice = h
            .app
            .nav_notice
            .as_ref()
            .expect("leaving says what it stopped");
        assert_eq!(notice.left as u8, Screen::Review as u8);
        assert_eq!(notice.receipts.len(), 1);
        assert_eq!(notice.receipts[0].pid, pid);
        assert_eq!(
            notice.resumable, 1,
            "the id was generated up front, so it can be continued"
        );

        wait_until_stopped(&mut h.app);
        let notice = h.app.nav_notice.as_ref().unwrap();
        assert!(
            notice.all_confirmed(),
            "the banner must not claim a kill it has not seen land"
        );
        assert!(
            notice.headline().contains("1/1 process(es) terminated"),
            "{}",
            notice.headline()
        );
        assert_eq!(h.app.procs.running_total(), 0);

        // The card is paused, not failed: the kill was ours, and the session
        // is still there to go back to.
        match &h.app.candidates[0] {
            CandidateState::Paused(p) => {
                assert_eq!(p.pid, pid);
                assert!(p.session.is_some(), "a generated id survives the kill");
                assert!(p.resumable(&h.app.settings));
            }
            other => panic!("expected a paused candidate, got {}", state_name(other)),
        }
    }

    /// Coming back must show what was paused rather than quietly starting a
    /// second conversation — the whole point of pausing.
    #[test]
    fn coming_back_shows_the_paused_session_and_starts_nothing() {
        let dir = TempDir::new("back_review");
        let (cli, model_config) = slow_resumable(&dir, "slow");
        let mut h = Harness::new("back_review");
        h.app.settings.prefetch_next = false;
        h.enter_with(vec![model_config]);
        wait_until_running(&mut h.app);
        h.app.goto(Screen::FilePicker);
        wait_until_stopped(&mut h.app);
        let session = match &h.app.candidates[0] {
            CandidateState::Paused(p) => p.session.clone().expect("a session to go back to"),
            other => panic!("expected paused, got {}", state_name(other)),
        };
        let argv_before = cli.argv_seen();

        // Back the way the file picker's own button goes: no plan movement,
        // no re-ask.
        h.app.goto(Screen::Review);
        assert_eq!(h.app.screen as u8, Screen::Review as u8);
        assert_eq!(
            h.app.procs.running_total(),
            0,
            "returning must not launch anything"
        );
        assert!(matches!(h.app.candidates[0], CandidateState::Paused(_)));
        assert_eq!(cli.argv_seen(), argv_before, "the CLI was not run again");

        // And even the file picker's "start review", which does move the plan,
        // will not throw the paused session away behind the reviewer's back.
        h.app.start_review(&egui::Context::default(), 0);
        assert!(matches!(h.app.candidates[0], CandidateState::Paused(_)));
        assert_eq!(h.app.procs.running_total(), 0);

        // Resuming is the reviewer's decision, and it goes back to the same
        // conversation rather than opening a new one.
        h.app.resume_candidate(&egui::Context::default(), 0);
        let argv = wait_for_argv(&cli, "--resume");
        assert!(
            argv.contains(&session),
            "resumed some other conversation: {argv}"
        );
        h.app.stop_all_models("end of test");
        wait_until_stopped(&mut h.app);
    }

    /// The reply our own kill produces must not land on the card as a model
    /// failure, and must not be scored as one either.
    #[test]
    fn a_stopped_call_is_not_recorded_as_a_model_error() {
        let dir = TempDir::new("stopped_not_error");
        let (_cli, model_config) = slow_resumable(&dir, "slow");
        let mut h = Harness::new("stopped_not_error");
        h.app.settings.prefetch_next = false;
        h.enter_with(vec![model_config]);
        wait_until_running(&mut h.app);
        h.app.goto(Screen::FilePicker);
        wait_until_stopped(&mut h.app);

        // The killed call's reply has arrived by now and left the card alone.
        assert!(
            matches!(h.app.candidates[0], CandidateState::Paused(_)),
            "our own kill overwrote the paused card"
        );
        // It is on the record — the tokens were spent — but marked, so the
        // leaderboard never counts walking away as a model that errors.
        let scored = h.app.db.agreement_rows().len();
        assert_eq!(scored, 0, "a stopped call must not reach the scoring join");
        let log = activity(&h);
        assert!(
            log.contains("stopped"),
            "the stop belongs in the activity log:
{log}"
        );
    }

    /// Every page that drives a CLI gets the same treatment, and stopping one
    /// page's work leaves the others alone.
    #[test]
    fn each_page_owns_its_processes_and_only_its_own() {
        let dir = TempDir::new("owners");
        let (_cli, model_config) = slow_resumable(&dir, "slow");
        let mut h = Harness::new("owners");
        h.app.settings.prefetch_next = false;
        h.enter_with(vec![model_config]);
        wait_until_running(&mut h.app);
        assert_eq!(h.app.procs.running(crate::procs::Owner::Review), 1);

        // Leaving for the summary screen ends the review's work and starts
        // none of its own.
        h.app.goto(Screen::Summary);
        wait_until_stopped(&mut h.app);
        assert_eq!(h.app.procs.running(crate::procs::Owner::Review), 0);

        // The branch pass is the summary's own work, and leaving the summary
        // is what ends it.
        h.app.start_whole_branch_review(&egui::Context::default());
        wait_until_running(&mut h.app);
        assert_eq!(h.app.procs.running(crate::procs::Owner::Branch), 1);
        h.app.goto(Screen::RepoPicker);
        wait_until_stopped(&mut h.app);
        assert_eq!(h.app.procs.running_total(), 0);
        match &h.app.whole_branch_review[0] {
            WholeBranchReviewState::Paused(p) => assert!(p.pid.is_some()),
            _ => panic!("the branch pass should be paused, not lost"),
        }
    }

    /// A row stopped from the ledger must become paused on the page that
    /// launched it. Branch and fix calls also have their own conversation
    /// ids; borrowing the same model's review id would resume the wrong chat.
    #[test]
    fn stopping_one_ledger_row_pauses_its_owner_without_stealing_a_review_session() {
        let dir = TempDir::new("one_row_owner");
        let cli = FakeCli::new(&dir, "owner", FakeCliSpec::default());
        let mut model = cli.model_config("");
        model.resume_command = format!("{} --review {{session}}", cli.command());
        model.fix_resume_command = format!("{} --fix {{session}}", cli.command());

        let mut h = Harness::new("one_row_owner");
        h.app.settings.models = vec![model];
        h.app.sessions = vec![Some("review-session".into())];

        let branch = h
            .app
            .procs
            .register(Owner::Branch, 0, "fake", "branch review");
        h.app.whole_branch_review = vec![WholeBranchReviewState::Pending(branch)];
        let branch_id = h
            .app
            .procs
            .live()
            .find(|row| row.owner == Owner::Branch)
            .unwrap()
            .id;
        assert_eq!(h.app.stop_model(branch_id, "test stop"), 1);
        match &h.app.whole_branch_review[0] {
            WholeBranchReviewState::Paused(call) => {
                assert!(matches!(call.owner, Owner::Branch));
                assert_eq!(call.session, None, "review session leaked into branch call");
            }
            _ => panic!("the branch row should be visibly paused"),
        }

        let fix = h.app.procs.register(Owner::Fix, 0, "fake", "fix turn");
        h.app.fix_proc = Some(fix);
        h.app.fix_running = true;
        h.app.fix_session = Some("fix-session".into());
        let fix_id = h
            .app
            .procs
            .live()
            .find(|row| row.owner == Owner::Fix)
            .unwrap()
            .id;
        assert_eq!(h.app.stop_model(fix_id, "test stop"), 1);
        let call = h.app.fix_paused.as_ref().expect("fix row should be paused");
        assert!(matches!(call.owner, Owner::Fix));
        assert_eq!(call.session.as_deref(), Some("fix-session"));
        assert!(call.resumable(&h.app.settings));
        assert!(!h.app.fix_running);
        assert!(h.app.fix_proc.is_none());
    }

    /// The spend a killed call had already run up stays on the books, or a
    /// reviewer who walks away often would never reach a limit they had set.
    #[test]
    fn a_usage_limit_stops_new_calls_and_counts_stopped_ones() {
        let dir = TempDir::new("limit");
        let reply = concat!(
            "{\"type\":\"result\",\"total_cost_usd\":2.5,",
            "\"usage\":{\"input_tokens\":1000,\"output_tokens\":200},",
            "\"result\":\"{\\\"action\\\":\\\"keep\\\",\\\"comment\\\":\\\"\\\"}\"}"
        );
        let cli = FakeCli::new(
            &dir,
            "pricey",
            FakeCliSpec {
                reply,
                ..Default::default()
            },
        );
        let mut h = Harness::new("limit");
        h.app.settings.prefetch_next = false;
        h.app.settings.usage_limit_usd = 2.0;
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.procs.sweep();

        assert_eq!(h.app.procs.spent().cost_usd, Some(2.5));
        assert_eq!(h.app.procs.spent().tokens(), 1200);
        let why = h
            .app
            .usage_block()
            .expect("$2.50 spent against a $2.00 ceiling");
        assert!(why.contains("usage limit reached"), "{why}");
        assert!(
            why.contains("settings"),
            "the way out has to be named: {why}"
        );

        // With the ceiling reached, the next unit is not queried at all.
        h.app.choose_keep();
        h.app.save_and_continue(&egui::Context::default(), false);
        assert_eq!(
            h.app.procs.running_total(),
            0,
            "a limit that still launches calls is not one"
        );
        assert!(matches!(h.app.candidates[0], CandidateState::Disabled));

        // Follow-ups are model calls too. Reaching the ceiling must leave the
        // typed question intact and launch no resumed conversation.
        h.app.candidates[0] = CandidateState::Ready(Suggestion {
            action: Action::Keep,
            comment: String::new(),
            justification: "fine".into(),
            evidence: Vec::new(),
            latency_ms: 1,
        });
        h.app.sessions[0] = Some("review-session".into());
        h.app.candidate_models[0].resume_command = cli.command();
        h.app.follow_up = "why?".into();
        h.app.ask_followup(&egui::Context::default(), Some(0));
        assert_eq!(h.app.procs.running_total(), 0);
        assert_eq!(h.app.follow_up, "why?");
        assert!(h
            .app
            .review_error
            .as_deref()
            .is_some_and(|e| e.contains("usage limit")));

        // Raising it lets the work continue rather than needing a restart.
        h.app.settings.usage_limit_usd = 100.0;
        assert!(h.app.usage_block().is_none());
        h.app.requery_unit(&egui::Context::default());
        h.wait_for_model_replies();
        assert!(matches!(h.app.candidates[0], CandidateState::Ready(_)));
    }

    /// Quitting is leaving every page at once, and the one departure with no
    /// window left to report an orphan in. The close has to be held back until
    /// the CLIs are actually dead: the threads that kill them die with the
    /// process, so exiting on the first request would leave them running.
    #[test]
    fn closing_waits_for_the_processes_to_actually_die() {
        let dir = TempDir::new("quit");
        let (_cli, model_config) = slow_resumable(&dir, "slow");
        let mut h = Harness::new("quit");
        h.enter_with(vec![model_config]);
        wait_until_running(&mut h.app);
        assert!(h.app.procs.running_total() > 0);

        // One frame in which the window manager asks to close.
        let ctx = egui::Context::default();
        let out = ctx.run(close_request(), |ctx| h.app.handle_close(ctx));
        assert!(h.app.closing, "the close should have been taken up");
        assert!(
            out.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose),
            "the window must be held open while the CLIs are still alive"
        );
        assert!(
            !out.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close),
            "closing before the kills land is exactly the orphan case"
        );

        // Frames keep coming while the kills land; the last one closes.
        wait_until_stopped(&mut h.app);
        let out = ctx.run(egui::RawInput::default(), |ctx| h.app.handle_close(ctx));
        assert!(
            out.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close),
            "with nothing left running the app should finally close"
        );
        assert_eq!(h.app.procs.running_total(), 0);
    }

    /// A `RawInput` carrying the window manager's close request.
    fn close_request() -> egui::RawInput {
        let mut raw = egui::RawInput::default();
        raw.viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .events
            .push(egui::ViewportEvent::Close);
        raw
    }

    /// A conversation outlives the run of the app that opened it — the CLI
    /// still holds it — so the id has to be written down or it is unreachable.
    #[test]
    fn a_paused_conversation_is_written_down_for_the_next_run() {
        let dir = TempDir::new("session_ledger");
        let (_cli, model_config) = slow_resumable(&dir, "slow");
        let mut h = Harness::new("session_ledger");
        h.app.settings.prefetch_next = false;
        h.enter_with(vec![model_config]);
        wait_until_running(&mut h.app);
        h.app.goto(Screen::FilePicker);
        wait_until_stopped(&mut h.app);
        let rows = h.app.earlier_paused_sessions();
        assert_eq!(
            rows.len(),
            1,
            "the paused conversation should be on the record"
        );
        assert_eq!(rows[0].owner, "review");
        let live = match &h.app.candidates[0] {
            CandidateState::Paused(p) => p.session.clone().unwrap(),
            other => panic!("expected paused, got {}", state_name(other)),
        };
        assert_eq!(
            rows[0].session, live,
            "the row must name the conversation the card offers"
        );
    }

    #[test]
    fn entering_a_unit_loads_the_comment_dedented() {
        let mut h = Harness::new("enter");
        h.enter_with(vec![]);
        assert_eq!(h.app.original_text, "    // Increment the counter by one");
        // The editor works flush left; the indent goes back on at save time.
        assert_eq!(h.app.original_display, "// Increment the counter by one");
        assert_eq!(h.app.editor, h.app.original_display);
        assert_eq!(h.app.screen as u8, Screen::Review as u8);
    }

    #[test]
    fn a_model_reply_becomes_a_pickable_candidate() {
        let dir = TempDir::new("reply");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("reply");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();

        match &h.app.candidates[0] {
            CandidateState::Ready(s) => assert_eq!(s.action, Action::Rewrite),
            other => panic!("expected a ready candidate, got {}", state_name(other)),
        }
        // Picking it must reformat the prose as a comment in the unit's style.
        h.app.choose_candidate(0);
        assert_eq!(h.app.editor, "// Counts retries.");
        assert_eq!(h.app.chosen, Some(Choice::Candidate(0)));
    }

    #[test]
    fn the_models_are_started_inside_the_repository_under_review() {
        let dir = TempDir::new("cwd");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("cwd");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();

        // Without this the prompt's file paths point at wherever the app was
        // launched from, and a model that goes looking finds someone else's
        // code — or nothing at all.
        let seen = std::fs::canonicalize(cli.cwd_seen()).expect("child cwd");
        let want = std::fs::canonicalize(h.repo.path()).expect("repo path");
        assert_eq!(seen, want);

        // The follow-up has to return to the same conversation, or the second turn
        // would be reasoning about a different tree than the first.
        std::fs::remove_file(dir.path().join("fake.cwd")).ok();
        h.app.sessions[0] = Some("s-1".into());
        h.app.candidate_models[0].resume_command =
            format!("{} --resume {{session}}", cli.command());
        h.app.follow_up = "why?".into();
        h.app.ask_followup(&egui::Context::default(), None);
        h.wait_for_model_replies();
        let resumed = std::fs::canonicalize(cli.cwd_seen()).expect("follow-up cwd");
        assert_eq!(resumed, want);
    }

    fn state_name(s: &CandidateState) -> &'static str {
        match s {
            CandidateState::Disabled => "disabled",
            CandidateState::Pending(_) => "pending",
            CandidateState::Paused(_) => "paused",
            CandidateState::Ready(_) => "ready",
            CandidateState::Failed(_) => "failed",
        }
    }

    #[test]
    fn a_session_id_is_captured_and_replayed_on_the_follow_up() {
        let dir = TempDir::new("session");
        let first = FakeCli::new(
            &dir,
            "first",
            FakeCliSpec {
                reply: "{\"conversation_id\":\"sess-7\",\"response\":\"\
{\\\"action\\\":\\\"keep\\\",\\\"justification\\\":\\\"fine\\\"}\"}",
                ..Default::default()
            },
        );
        let second = FakeCli::new(
            &dir,
            "second",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );

        let mut h = Harness::new("session");
        let mut model = first.model_config("");
        model.session_key = "conversation_id".into();
        model.resume_command = format!("{} --conversation {{session}}", second.command());
        h.enter_with(vec![model]);
        h.wait_for_model_replies();

        assert_eq!(h.app.sessions[0].as_deref(), Some("sess-7"));
        assert!(
            h.app.can_ask(0),
            "a model that replied with a session should accept a follow-up"
        );

        h.app.follow_up = "too vague".into();
        h.app.ask_followup(&egui::Context::default(), None);
        h.wait_for_model_replies();

        // The id must reach the resumed process, and only the new message with
        // it — the conversation itself lives in the CLI's session.
        let argv = second.argv_seen();
        assert!(argv.contains("--conversation sess-7"), "{argv}");
        let sent = second.stdin_seen();
        assert!(sent.contains("too vague"), "{sent}");
        assert!(
            !sent.contains("Increment the counter"),
            "the diff was re-sent: {sent}"
        );
        assert!(h.app.follow_up.is_empty(), "the box should clear once sent");
        assert_eq!(
            h.app.convos[0].len(),
            2,
            "both turns belong in the inspector"
        );
    }

    #[test]
    fn a_follow_up_is_recorded_in_full_and_its_answers_carry_it() {
        let dir = TempDir::new("fu_record");
        let first = FakeCli::new(
            &dir,
            "first",
            FakeCliSpec {
                reply: "{\"conversation_id\":\"sess-9\",\"response\":\"\
{\\\"action\\\":\\\"keep\\\",\\\"justification\\\":\\\"fine\\\"}\"}",
                ..Default::default()
            },
        );
        let second = FakeCli::new(
            &dir,
            "second",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("fu_record");
        h.app.settings.prefetch_next = false;
        let mut model = first.model_config("");
        model.session_key = "conversation_id".into();
        model.resume_command = format!("{} --conversation {{session}}", second.command());
        h.enter_with(vec![model]);
        h.wait_for_model_replies();

        let long = "Is the second sentence necessary? Maintainers can read the code, and the \
                    activity log keeps only eighty characters of this, which is the point.";
        h.app.follow_up = long.into();
        h.app.ask_followup(&egui::Context::default(), None);
        h.wait_for_model_replies();

        let links = h.app.db.suggestion_links();
        assert_eq!(links.len(), 2, "one opening answer, one follow-up answer");
        assert_eq!(
            (links[0].2, links[0].3),
            (None, Some(1)),
            "the opening round has no question"
        );
        let fu_id = links[1]
            .2
            .expect("the answer should carry its question's id");
        assert_eq!(
            links[1].3,
            Some(2),
            "the answer to the first question is round two"
        );
        let (round, question) = h.app.db.followup_row(fu_id).expect("the question row");
        assert_eq!(round, 2);
        assert_eq!(
            question, long,
            "the question must be stored whole, not truncated"
        );
    }

    #[test]
    fn the_next_unit_is_prefetched_and_adopted_without_asking_again() {
        let dir = TempDir::new("prefetch");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("prefetch");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.wait_for_prefetch_replies();
        assert_eq!(
            h.app.prefetches.len(),
            1,
            "the second unit should have been prefetched"
        );

        h.app.choose_keep();
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(
            matches!(h.app.candidates[0], CandidateState::Ready(_)),
            "the prefetched verdict should be waiting the moment the review advances"
        );
        assert_eq!(
            h.app.db.suggestion_links().len(),
            2,
            "each unit's answer is recorded exactly once"
        );
    }

    #[test]
    fn a_unanimous_keep_is_deferred_until_after_the_contested_units() {
        let dir = TempDir::new("defer");
        let cli = FakeCli::new(
            &dir,
            "keeper",
            FakeCliSpec {
                reply: "{\"action\":\"keep\",\"justification\":\"fine\"}",
                ..Default::default()
            },
        );
        let mut h = Harness::new("defer");
        // A third comment, so the deferred second unit has something to yield to.
        h.repo.write(
            "src/lib.rs",
            concat!(
                "fn main() {\n",
                "    // Increment the counter by one\n",
                "    counter += 1;\n",
                "    // Reset the counter to zero\n",
                "    counter = 0;\n",
                "    // Close the file handle\n",
                "    handle.close();\n",
                "}\n",
            ),
        );
        h.repo.commit("third comment");
        let session_id = h.app.plan.as_ref().unwrap().session_id;
        h.app.plan = Some(Harness::plan(&h.repo, session_id));
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.wait_for_prefetch_replies();

        h.app.choose_keep();
        h.app.save_and_continue(&egui::Context::default(), false);
        let unit = h.app.current_unit().expect("a unit on screen");
        assert_eq!(
            unit.start_line(),
            6,
            "the all-keep second unit should step aside for the third"
        );
        assert!(
            activity(&h).contains("deferred"),
            "the reorder must be visible in the log"
        );

        // The deferred unit is still reviewed — last, from its stored answers.
        h.wait_for_model_replies();
        h.wait_for_prefetch_replies();
        h.app.choose_keep();
        h.app.save_and_continue(&egui::Context::default(), false);
        let unit = h.app.current_unit().expect("the deferred unit comes back");
        assert_eq!(unit.start_line(), 4);
        assert!(
            matches!(h.app.candidates[0], CandidateState::Ready(_)),
            "its stored verdict should be installed, not re-queried"
        );
    }

    #[test]
    fn standing_guidance_reaches_the_prompt_mined_or_written() {
        let dir = TempDir::new("profile_prompt");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("profile_prompt");
        h.app.settings.prefetch_next = false;
        h.app
            .db
            .log_follow_up(1, "src/other.rs", 3, 3, 2, "Say why, not what.");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        let sent = cli.stdin_seen();
        assert!(sent.contains("Reviewer preferences"), "{sent}");
        assert!(sent.contains("Say why, not what."), "{sent}");

        // Switched off, the preamble stays home.
        h.app.settings.send_profile = false;
        h.app.start_review(&egui::Context::default(), 0);
        h.wait_for_model_replies();
        assert!(!cli.stdin_seen().contains("Reviewer preferences"));

        // Written in settings, that text is sent instead of the mined one.
        h.app.settings.send_profile = true;
        h.app.settings.reviewer_preferences = "Only judge what the diff changed.".into();
        h.app.start_review(&egui::Context::default(), 0);
        h.wait_for_model_replies();
        let sent = cli.stdin_seen();
        assert!(sent.contains("Only judge what the diff changed."), "{sent}");
        assert!(!sent.contains("Say why, not what."), "{sent}");
    }

    #[test]
    fn a_model_without_a_session_cannot_be_asked_again() {
        let dir = TempDir::new("nosession");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("nosession");
        // No session key and no {session} in the command: nothing to resume.
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();

        assert!(!h.app.can_ask(0));
        h.app.follow_up = "why?".into();
        h.app.ask_followup(&egui::Context::default(), None);
        assert!(
            h.app.review_error.is_some(),
            "the user needs to be told why nothing happened"
        );
        assert_eq!(
            h.app.follow_up, "why?",
            "an unsent message must not be cleared"
        );
    }

    #[test]
    fn a_pending_model_is_not_asked_again_while_running() {
        let dir = TempDir::new("pending");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                delay_secs: 30,
                ..Default::default()
            },
        );
        let mut h = Harness::new("pending");
        let mut model = cli.model_config("");
        model.session_key = "conversation_id".into();
        model.resume_command = format!("{} {{session}}", cli.command());
        h.enter_with(vec![model]);

        // Still running: one reply per request keeps answers attributable.
        assert!(matches!(h.app.candidates[0], CandidateState::Pending(_)));
        assert!(!h.app.can_ask(0));
    }

    #[test]
    fn saving_writes_the_indent_back_and_records_provenance() {
        let dir = TempDir::new("save");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("save");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);

        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);

        let after = h.repo.read("src/lib.rs");
        assert!(
            after.contains("    // Counts retries."),
            "indent not restored: {after}"
        );
        assert!(!after.contains("Increment the counter"), "{after}");
        // And it moved on to the next comment in the same file.
        assert_eq!(h.app.original_display, "// Reset the counter to zero");
    }

    #[test]
    fn a_second_edit_in_the_same_file_accounts_for_the_first() {
        let dir = TempDir::new("offset");
        let two_liner = "{\"action\":\"rewrite\",\"comment\":\"First line.\\nSecond line.\",\
\"justification\":\"needs two\"}";
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: two_liner,
                ..Default::default()
            },
        );
        let mut h = Harness::new("offset");

        // First comment becomes two lines, pushing everything below it down.
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert_eq!(h.app.plan.as_ref().unwrap().files[0].edits, vec![(2, 1)]);

        // The second edit has to be applied to the shifted lines, not the original.
        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);

        let after = h.repo.read("src/lib.rs");
        assert_eq!(
            after.matches("// First line.").count(),
            2,
            "both comments rewritten: {after}"
        );
        assert!(after.contains("counter += 1;"), "{after}");
        assert!(after.contains("counter = 0;"), "{after}");
        assert!(!after.contains("Increment"), "{after}");
        assert!(!after.contains("Reset"), "{after}");
        assert_eq!(
            h.app.screen as u8,
            Screen::Summary as u8,
            "plan should be exhausted"
        );
    }

    #[test]
    fn an_external_edit_above_the_unit_relocates_the_save() {
        let dir = TempDir::new("drift");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("drift");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);

        // Someone touches the file while the review is on screen: two lines
        // added above everything the plan knows about.
        let on_disk = h.repo.read("src/lib.rs");
        h.repo.write(
            "src/lib.rs",
            &format!("// header\n// more header\n{on_disk}"),
        );

        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(
            h.app.review_error.is_none(),
            "a pure drift must resolve itself: {:?}",
            h.app.review_error
        );
        let after = h.repo.read("src/lib.rs");
        assert!(after.contains("    // Counts retries."), "{after}");
        assert!(!after.contains("Increment"), "{after}");
        assert!(
            after.starts_with("// header\n"),
            "the outside edit must survive: {after}"
        );

        // The measured drift carries to the next unit in the file: its save
        // is applied without needing another relocation.
        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        let after = h.repo.read("src/lib.rs");
        assert!(!after.contains("Reset"), "{after}");
        assert!(after.contains("counter = 0;"), "{after}");
    }

    #[test]
    fn a_unit_changed_on_disk_offers_reload_and_reviews_the_new_text() {
        let dir = TempDir::new("stale");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("stale");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);

        // The unit's own line was rewritten outside the app: no amount of
        // relocating can make the old snapshot true again.
        let on_disk = h.repo.read("src/lib.rs");
        h.repo.write(
            "src/lib.rs",
            &on_disk.replace(
                "    // Increment the counter by one",
                "    // Bump the counter (edited elsewhere)",
            ),
        );

        h.app.save_and_continue(&egui::Context::default(), false);
        let err = h
            .app
            .review_error
            .clone()
            .expect("the mismatch must be surfaced");
        assert!(err.contains("changed on disk"), "{err}");
        let stale = h
            .app
            .stale_unit
            .as_ref()
            .expect("resolution state must be offered");
        assert_eq!(
            stale.lines,
            vec!["    // Bump the counter (edited elsewhere)".to_string()]
        );
        // Nothing was written while the question is open.
        assert!(h.repo.read("src/lib.rs").contains("Bump the counter"));

        // Taking the reload reviews what is actually there now.
        h.app.reload_stale_unit(&egui::Context::default());
        assert!(
            h.app.stale_unit.is_none(),
            "the panel must clear once resolved"
        );
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert_eq!(
            h.app.original_display,
            "// Bump the counter (edited elsewhere)"
        );
        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        let after = h.repo.read("src/lib.rs");
        assert!(after.contains("    // Counts retries."), "{after}");
        assert!(!after.contains("Bump the counter"), "{after}");
    }

    #[test]
    fn committing_records_the_model_as_co_author() {
        let dir = TempDir::new("commit");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("commit");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);

        h.app.save_and_continue(&egui::Context::default(), true);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);

        let message = h.repo.git(&["log", "-1", "--format=%B"]);
        assert!(message.contains("review(comments): rewrite"), "{message}");
        assert!(message.contains("Comment-provenance: fake"), "{message}");
        assert!(
            message.contains("Co-authored-by: Fake <fake@example.com>"),
            "{message}"
        );
        assert!(
            message.contains("says why"),
            "the model's reasoning belongs in the body: {message}"
        );
    }

    #[test]
    fn commit_message_records_model_and_effort() {
        let dir = TempDir::new("model-effort");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut model = cli.model_config("");
        model.model = "claude-sonnet-5".into();
        model.effort = "low".into();
        let mut h = Harness::new("model-effort");
        h.enter_with(vec![model]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);

        h.app.save_and_continue(&egui::Context::default(), true);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);

        let message = h.repo.git(&["log", "-1", "--format=%B"]);
        assert!(message.contains("Model: claude-sonnet-5"), "{message}");
        assert!(message.contains("Effort: low"), "{message}");
    }

    /// The bug this app was filed over: a `Save and Continue` on one comment
    /// followed by `Commit and Continue` on the next must not lose the first
    /// decision's justification from the commit that ends up covering both.
    #[test]
    fn batched_saves_are_all_documented_in_the_final_commit() {
        let dir = TempDir::new("batch");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("batch");
        let before = gitio::head_sha(&h.repo.path()).unwrap();
        h.enter_with(vec![cli.model_config("")]);

        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert_eq!(
            gitio::head_sha(&h.repo.path()).unwrap(),
            before,
            "save must not commit"
        );

        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), true);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);

        // Only one commit for both decisions...
        let new_commits = h
            .repo
            .git(&["rev-list", "--count", &format!("{before}..HEAD")]);
        assert_eq!(
            new_commits.trim(),
            "1",
            "the batch should create one commit"
        );

        // ...whose message documents both, not just the second.
        let message = h.repo.git(&["log", "-1", "--format=%B"]);
        assert!(message.contains("2 comment decisions"), "{message}");
        assert!(message.contains("src/lib.rs:2"), "{message}");
        assert!(message.contains("src/lib.rs:4"), "{message}");
        assert!(
            message.matches("says why").count() == 2,
            "each decision's justification: {message}"
        );
        // Trailers are shared, not duplicated once per decision.
        assert_eq!(message.matches("Co-authored-by:").count(), 1, "{message}");

        assert!(
            h.app.pending.is_empty(),
            "the flushed decisions must leave pending empty"
        );
    }

    /// A working-tree review of an untracked file: approving its hunks with
    /// the commit toggle on must actually commit the file. A keep normally
    /// commits nothing — right for branch reviews, where the kept lines are
    /// already committed, but here the file would stay untracked forever.
    #[test]
    fn a_keep_commit_on_a_working_tree_review_commits_the_untracked_file() {
        let mut h = Harness::new("keep-untracked");
        h.repo.write("src/new_mod.rs", LIB_RS);

        let diff = gitio::review_diff(&h.repo.path(), gitio::UNTRACKED, 12).expect("diff");
        let extracted = crate::comments::extract_units(&crate::diffparse::parse(&diff), 12);
        let (path, units) = extracted
            .into_iter()
            .find(|(p, _)| p == "src/new_mod.rs")
            .expect("the untracked file must be reviewable");
        h.app.plan = Some(ReviewPlan {
            session_id: 1,
            ref_kind: RefKind::WorkingTree,
            ref_name: "feature".into(),
            base_ref: "HEAD+untracked".into(),
            branch_base: gitio::head_sha(&h.repo.path()).unwrap(),
            files: vec![ReviewFile::new(
                path,
                units.into_iter().map(ReviewUnit::Comment).collect(),
            )],
            file_idx: 0,
            unit_idx: 0,
            decided_total: 0,
            skipped_decided: 0,
        });
        h.app.commit_each = true;
        h.enter_with(vec![]);

        h.app.choose_keep();
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert!(
            gitio::untracked_files(&h.repo.path()).unwrap().is_empty(),
            "the approved file is still untracked"
        );
        let last = h.repo.git(&["log", "-1", "--name-only", "--format=%B"]);
        assert!(last.contains("keep"), "{last}");
        assert!(last.contains("src/new_mod.rs"), "{last}");

        // The whole file went into that commit, so the second hunk's keep has
        // nothing left to do — and must say so rather than erroring out.
        let sha = gitio::head_sha(&h.repo.path()).unwrap();
        h.app.choose_keep();
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert_eq!(
            gitio::head_sha(&h.repo.path()).unwrap(),
            sha,
            "no empty second commit"
        );
    }

    #[test]
    fn commit_each_toggle_makes_every_save_its_own_commit() {
        let dir = TempDir::new("commit-each");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("commit-each");
        let before = gitio::head_sha(&h.repo.path()).unwrap();
        h.app.commit_each = true;
        h.enter_with(vec![cli.model_config("")]);

        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        // `Save and Continue` — not `Commit and Continue` — but the toggle
        // forces an immediate commit rather than batching.
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);

        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);

        let new_commits = h
            .repo
            .git(&["rev-list", "--count", &format!("{before}..HEAD")]);
        assert_eq!(
            new_commits.trim(),
            "2",
            "each decision should be its own commit"
        );
        assert!(h.app.pending.is_empty());

        let latest = h.repo.git(&["log", "-1", "--format=%B"]);
        assert!(
            latest.contains("review(comments): rewrite comment in src/lib.rs:4"),
            "{latest}"
        );
        assert!(
            !latest.contains("src/lib.rs:2"),
            "the first decision has its own commit: {latest}"
        );
    }

    #[test]
    fn keeping_the_original_touches_neither_the_file_nor_git() {
        let dir = TempDir::new("keep");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("keep");
        let before = h.repo.read("src/lib.rs");
        let head = gitio::head_sha(&h.repo.path()).unwrap();

        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_keep();
        h.app.save_and_continue(&egui::Context::default(), true);

        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert_eq!(
            h.repo.read("src/lib.rs"),
            before,
            "a keep must not rewrite the file"
        );
        assert_eq!(
            gitio::head_sha(&h.repo.path()).unwrap(),
            head,
            "a keep must not commit"
        );
    }

    #[test]
    fn a_failing_model_leaves_the_others_usable() {
        let dir = TempDir::new("mixed");
        let good = FakeCli::new(
            &dir,
            "good",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let bad = FakeCli::new(
            &dir,
            "bad",
            FakeCliSpec {
                reply: "boom",
                exit_code: 1,
                ..Default::default()
            },
        );
        let mut h = Harness::new("mixed");
        h.enter_with(vec![good.model_config(""), bad.model_config("")]);
        h.wait_for_model_replies();

        assert!(matches!(h.app.candidates[0], CandidateState::Ready(_)));
        assert!(matches!(h.app.candidates[1], CandidateState::Failed(_)));
        h.app.choose_candidate(0);
        assert_eq!(h.app.chosen, Some(Choice::Candidate(0)));
    }

    #[test]
    fn a_blind_choice_cannot_be_changed_after_names_are_revealed() {
        let dir = TempDir::new("blind");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("blind");
        h.app.settings.blind_review = true;
        let mut a = cli.model_config("");
        a.name = "alpha".into();
        let mut b = cli.model_config("");
        b.name = "beta".into();
        h.enter_with(vec![a, b]);
        h.wait_for_model_replies();

        assert!(h.app.names_hidden());
        let order = h.app.candidate_order();
        // Labels follow screen position, not model, so nothing identifies the
        // model behind a card.
        assert_eq!(h.app.model_label(order[0], 0), "model A");
        assert_eq!(h.app.model_label(order[1], 1), "model B");

        // Picking the first card must select whichever model it stands for.
        h.app.choose_candidate(order[0]);
        assert_eq!(h.app.chosen, Some(Choice::Candidate(order[0])));
        // Once chosen, names come back, but the newly revealed identities
        // must not let the reviewer shop for a different answer.
        assert!(!h.app.names_hidden());
        assert!(h.app.model_label(order[0], 0) != "model A");
        let chosen_text = h.app.editor.clone();
        h.app.choose_candidate(order[1]);
        h.app.choose_delete();
        assert_eq!(h.app.chosen, Some(Choice::Candidate(order[0])));
        assert_eq!(h.app.editor, chosen_text);
    }

    #[test]
    fn a_new_plan_resets_commit_mode_and_pending_decisions() {
        let mut h = Harness::new("session-reset");
        h.app.commit_each = true;
        h.app.pending.push((
            7,
            review::PendingDecision {
                file: "src/lib.rs".into(),
                line: 2,
                kind: units::UnitKind::Comment,
                action: Action::Rewrite,
                provenance: review::Provenance::Human,
                justification: None,
                model_info: None,
            },
        ));

        h.app
            .build_plan(RefKind::Branch, "feature".into(), "main".into());

        assert!(!h.app.commit_each);
        assert!(h.app.pending.is_empty());
    }

    #[test]
    fn a_working_tree_plan_keeps_its_starting_head_for_the_branch_pass() {
        let mut h = Harness::new("working-base");
        let starting_head = gitio::head_sha(&h.repo.path()).unwrap();
        h.repo.write(
            "src/lib.rs",
            &format!(
                "{}\nfn newly_added() {{ risky(); }}\n",
                h.repo.read("src/lib.rs")
            ),
        );

        h.app
            .build_plan(RefKind::WorkingTree, "feature".into(), String::new());
        let captured = h.app.plan.as_ref().unwrap().branch_base.clone();
        assert_eq!(captured, starting_head);

        // Committing during review advances HEAD. The captured SHA must still
        // produce the whole reviewed change for the later branch pass.
        h.repo.commit("review decision");
        let diff = gitio::review_diff(&h.repo.path(), &captured, 12).unwrap();
        assert!(diff.contains("newly_added"), "{diff}");
    }

    #[test]
    fn a_recheck_uses_only_the_selected_repository() {
        let mut h = Harness::new("recheck-repo");
        let selected_repo = h.repo.path();
        let selected_session = h
            .app
            .db
            .new_session(&selected_repo, "branch", "feature", "main");
        let other_session = h
            .app
            .db
            .new_session("C:/different/repo", "branch", "feature", "main");
        let unit = h.app.current_unit().unwrap();
        let unit_json = serde_json::to_string(&unit).unwrap();
        for session_id in [selected_session, other_session] {
            h.app.db.log_decision(&crate::db::DecisionRecord {
                session_id,
                file: unit.file(),
                line_start: unit.start_line(),
                line_end: unit.end_line(),
                original: &unit.raw_lines().join("\n"),
                action: "keep",
                final_text: &unit.raw_lines().join("\n"),
                source: "original",
                human_edited: false,
                committed: false,
                commit_sha: None,
                justification: None,
                unit_json: Some(&unit_json),
                blinded: true,
            });
        }

        h.app.settings.models.clear();
        h.app.start_recheck(&egui::Context::default(), 10);

        assert_eq!(h.app.plan.as_ref().unwrap().total_units(), 1);
    }

    /// Every line the ACTIVITY panel shows, combined into searchable text.
    fn activity(h: &Harness) -> String {
        h.app
            .log_lines
            .iter()
            .map(|(_, l)| l.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_activity_log_does_not_name_models_while_blinded() {
        let dir = TempDir::new("blindlog");
        let ok = FakeCli::new(
            &dir,
            "ok",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let bad = FakeCli::new(
            &dir,
            "bad",
            FakeCliSpec {
                reply: "nope",
                exit_code: 1,
                ..Default::default()
            },
        );
        let mut h = Harness::new("blindlog");
        h.app.settings.blind_review = true;
        let mut a = ok.model_config("");
        a.name = "alpha".into();
        let mut b = bad.model_config("");
        b.name = "beta".into();
        h.enter_with(vec![a, b]);
        h.wait_for_model_replies();

        // The panel is the same surface as the cards: a reply line that names
        // the model undoes the blinding the cards are enforcing, and a failure
        // line does it just as well as a success.
        let log = activity(&h);
        assert!(
            !log.contains("alpha"),
            "the log named a model while blinded:\n{log}"
        );
        assert!(
            !log.contains("beta"),
            "the log named a model while blinded:\n{log}"
        );
        assert!(
            log.contains("replied"),
            "the reply was never logged at all:\n{log}"
        );
        assert!(
            log.contains("failed"),
            "the failure was never logged at all:\n{log}"
        );
        assert!(log.contains("model A") || log.contains("model B"), "{log}");

        // Once a choice is made the name is the point — it is the provenance.
        let order = h.app.candidate_order();
        let ready = order
            .iter()
            .copied()
            .find(|&i| matches!(h.app.candidates.get(i), Some(CandidateState::Ready(_))))
            .expect("one model answered");
        h.app.choose_candidate(ready);
        let log = activity(&h);
        assert!(
            log.contains("picked alpha"),
            "the pick must be on the record by name:\n{log}"
        );
    }

    #[test]
    fn a_follow_up_is_logged_by_position_while_blinded() {
        let dir = TempDir::new("blindask");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: "{\"conversation_id\":\"sess-1\",\"response\":\"\
{\\\"action\\\":\\\"keep\\\",\\\"justification\\\":\\\"fine\\\"}\"}",
                ..Default::default()
            },
        );
        let mut h = Harness::new("blindask");
        h.app.settings.blind_review = true;
        let mut a = cli.model_config("");
        a.name = "alpha".into();
        a.session_key = "conversation_id".into();
        a.resume_command = format!("{} --resume {{session}}", cli.command());
        h.enter_with(vec![a]);
        h.wait_for_model_replies();
        assert!(
            h.app.can_ask(0),
            "the model must be resumable for this test to mean anything"
        );

        h.app.follow_up = "why?".into();
        h.app.ask_followup(&egui::Context::default(), None);

        // Which models were asked is as much of a tell as which one answered.
        let log = activity(&h);
        assert!(log.contains("asked model A"), "{log}");
        assert!(
            !log.contains("alpha"),
            "the follow-up named a model while blinded:\n{log}"
        );
    }

    #[test]
    fn blinding_off_leaves_the_order_and_the_names_alone() {
        let dir = TempDir::new("unblind");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("unblind");
        h.app.settings.blind_review = false;
        let mut a = cli.model_config("");
        a.name = "alpha".into();
        h.enter_with(vec![a]);

        assert!(!h.app.names_hidden());
        assert_eq!(h.app.candidate_order(), vec![0]);
        assert_eq!(h.app.model_label(0, 0), "alpha");
    }

    #[test]
    fn a_decision_records_whether_it_was_blinded() {
        let dir = TempDir::new("blindrec");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("blindrec");
        h.app.settings.blind_review = true;
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);

        // An unblinded label is weaker evidence, so the report has to be able
        // to tell them apart after the fact.
        let rows = h.app.db.corpus(10);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].blinded);
        assert!(
            !rows[0].unit_json.is_empty(),
            "the unit must be stored for replay"
        );
    }

    #[test]
    fn a_recheck_records_the_judgement_without_touching_the_file() {
        let dir = TempDir::new("recheck");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("recheck");

        // Decide one comment normally, which writes the file and stores a label.
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);
        let after_first = h.repo.read("src/lib.rs");
        let head = gitio::head_sha(&h.repo.path()).unwrap();
        assert_eq!(h.app.db.corpus(10).len(), 1);

        // Now re-judge it. The point is the verdict, not the edit.
        h.app.settings.models.clear();
        h.app.start_recheck(&egui::Context::default(), 10);
        assert!(h.app.plan.as_ref().unwrap().is_recheck());
        // The stored unit is the comment as it was *asked about*, not the
        // rewrite it became — re-judging the outcome would be a different
        // question and useless as a consistency measure.
        assert_eq!(h.app.original_display, "// Increment the counter by one");

        h.app.choose_keep();
        h.app.save_and_continue(&egui::Context::default(), true);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert_eq!(
            h.repo.read("src/lib.rs"),
            after_first,
            "a re-check must not edit the tree"
        );
        assert_eq!(gitio::head_sha(&h.repo.path()).unwrap(), head, "or commit");
        assert_eq!(
            h.app.db.corpus(10).len(),
            2,
            "but it must record the second judgement"
        );
    }

    #[test]
    fn a_recheck_with_no_history_explains_itself() {
        let mut h = Harness::new("nohistory");
        h.app.start_recheck(&egui::Context::default(), 10);
        assert!(h
            .app
            .ref_error
            .as_deref()
            .is_some_and(|e| e.contains("no past decisions")));
    }

    #[test]
    fn repeated_judgements_give_the_report_a_noise_floor() {
        let dir = TempDir::new("floor");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("floor");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);

        // Before any repeat there is no scale to read agreement against.
        assert!(crate::eval::Report::from_db(&h.app.db)
            .self_agreement_pct()
            .is_none());

        // Re-judge the same comment the same way.
        h.app.settings.models.clear();
        h.app.start_recheck(&egui::Context::default(), 10);
        h.app.choose_keep();
        h.app.save_and_continue(&egui::Context::default(), false);

        // Re-judging the same comment gives the report its scale. Here the
        // verdict changed (rewrite, then keep), so self-agreement is 0% — a
        // reviewer who contradicts themselves caps what any model can score.
        let report = crate::eval::Report::from_db(&h.app.db);
        assert_eq!(report.repeat_total, 1);
        assert_eq!(report.self_agreement_pct(), Some(0.0));
        assert!(
            report.render().contains("self-agreement: 0%"),
            "{}",
            report.render()
        );
    }

    // -- decisions persist across sessions -----------------------------------

    /// Build the plan the way the ref picker does, with only comment review
    /// on, so these tests watch the same two comment units each time.
    fn replan(h: &mut Harness) {
        h.app.settings.review_code = false;
        h.app
            .build_plan(RefKind::Branch, "feature".into(), "main".into());
    }

    /// The file picker's numbers come off the plan, so they have to be
    /// measured while the diff and the checkout are both in hand — by paint
    /// time neither is reachable.
    #[test]
    fn a_plan_records_what_changed_in_each_file_and_how_big_the_file_is() {
        let mut h = Harness::new("stats");
        replan(&mut h);
        let file = &h.app.plan.as_ref().unwrap().files[0];
        // The branch replaces a one-line file with a six-line one.
        assert_eq!(file.line_changes, (6, 1));
        assert_eq!(
            file.total_lines, 6,
            "the size is of the file on disk, not of the diff"
        );
        // Two one-line comment units. What the reviewer is asked to read is a
        // small part of what changed, which is why both numbers are shown.
        assert_eq!(file.review_lines(), 2);
    }

    #[test]
    fn a_decided_unit_is_not_offered_again_by_a_later_plan() {
        let mut h = Harness::new("decided");
        replan(&mut h);
        assert_eq!(h.app.plan.as_ref().unwrap().total_units(), 2);

        // Keeping a comment is a verdict too, even though nothing is written.
        h.enter_with(vec![]);
        let judged = h.app.current_unit().expect("a unit").raw_lines().join(
            "
",
        );
        h.app.save_and_continue(&egui::Context::default(), false);

        // Same branch, same diff, opened again later: one unit fewer.
        replan(&mut h);
        let plan = h.app.plan.as_ref().unwrap();
        assert_eq!(plan.total_units(), 1, "the decided unit came back");
        assert_eq!(plan.skipped_decided, 1);
        assert!(
            plan.files[0].units.iter().all(|u| u.raw_lines().join(
                "
"
            ) != judged),
            "the plan still holds the unit that was already judged"
        );
    }

    #[test]
    fn a_committed_rewrite_is_not_offered_back_as_new_work() {
        let mut h = Harness::new("rewritten");
        replan(&mut h);

        // Rewrite the first comment and commit it. The branch diff now shows
        // the *new* text, which is a unit the extractor has never seen — only
        // recognising it as the outcome of a decision keeps it out.
        h.enter_with(vec![]);
        h.app.editor = "// Counts retries.".into();
        h.app.save_and_continue(&egui::Context::default(), true);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert!(
            h.repo.read("src/lib.rs").contains("// Counts retries."),
            "the rewrite never reached the file"
        );

        replan(&mut h);
        let plan = h.app.plan.as_ref().unwrap();
        assert_eq!(plan.skipped_decided, 1);
        assert!(
            plan.files[0].units.iter().all(|u| !u
                .raw_lines()
                .join(
                    "
"
                )
                .contains("Counts retries")),
            "the rewrite is being reviewed as if it were someone else's comment"
        );
    }

    #[test]
    fn turning_the_setting_off_reviews_everything_again() {
        let mut h = Harness::new("nodecided");
        replan(&mut h);
        h.enter_with(vec![]);
        h.app.save_and_continue(&egui::Context::default(), false);

        h.app.settings.skip_decided = false;
        replan(&mut h);
        let plan = h.app.plan.as_ref().unwrap();
        assert_eq!(
            plan.total_units(),
            2,
            "the toggle should have restored both units"
        );
        assert_eq!(plan.skipped_decided, 0);
    }

    /// A plan with nothing left normally reopens the finished session on the
    /// summary — but that needs a session to reopen. Decisions are recorded
    /// per repository, so they can outlive the ref name they were made under
    /// (an older database, a branch since renamed). Then there is genuinely
    /// nothing to reopen, and the message has to carry the explanation on its
    /// own rather than leaving an empty plan looking like a lost extractor.
    #[test]
    fn a_plan_with_nothing_left_and_no_session_to_reopen_says_so() {
        let mut h = Harness::new("alldecided");
        replan(&mut h);
        // One review through the whole plan: save advances to the next unit.
        h.enter_with(vec![]);
        let ctx = egui::Context::default();
        h.app.save_and_continue(&ctx, false);
        h.app.save_and_continue(&ctx, false);
        assert_eq!(h.app.plan.as_ref().unwrap().decided_total, 2);

        // The same diff under a name no session was ever opened for.
        h.app.settings.review_code = false;
        h.app
            .build_plan(RefKind::Branch, "renamed-since".into(), "main".into());
        let err = h.app.ref_error.as_deref().unwrap_or_default();
        assert!(err.contains("already"), "unhelpful message: {err}");
        assert!(err.contains('2'), "the message should count them: {err}");
        assert!(
            err.contains("no review session"),
            "and say why it stopped here: {err}"
        );
    }

    #[test]
    fn another_repository_keeps_its_own_review_to_itself() {
        let mut h = Harness::new("otherrepo");
        replan(&mut h);
        h.enter_with(vec![]);
        h.app.save_and_continue(&egui::Context::default(), false);

        // A second checkout with byte-identical comments: its own review.
        let other = TempRepo::new("otherrepo2");
        other.write(
            "src/lib.rs",
            "fn main() {}
",
        );
        other.commit("base");
        other.git(&["checkout", "-b", "feature"]);
        other.write("src/lib.rs", LIB_RS);
        other.commit("add counter");
        h.app.repo = Some(RepoCtx {
            path: other.path(),
            name: "other-repo".into(),
            default_branch: "main".into(),
        });
        replan(&mut h);
        let plan = h.app.plan.as_ref().unwrap();
        assert_eq!(
            plan.total_units(),
            2,
            "a verdict leaked across repositories"
        );
        assert_eq!(plan.skipped_decided, 0);
    }

    // -- code units ---------------------------------------------------------

    const CODE_LIB_RS: &str = "fn main() {\n    let count = 1;\n    print(count);\n}\n";

    const REVISE: &str = "{\"action\":\"revise\",\
\"replacement\":\"    let count = 2;\\n    print(count);\",\
\"justification\":\"start at two\",\
\"evidence\":[{\"file\":\"src/lib.rs\",\"lines\":\"1-4\",\"note\":\"read the whole fn\"}]}";

    /// A harness whose branch changes code, planned through the same
    /// assembly the ref picker uses, with only code review enabled.
    fn code_harness(tag: &str) -> Harness {
        let dir = TempDir::new(tag);
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let mut app = CraApp::with_db(db);
        app.settings.models.clear();

        let repo = TempRepo::new(tag);
        repo.write("src/lib.rs", "fn main() {\n    print(0);\n}\n");
        repo.commit("base");
        repo.git(&["checkout", "-b", "feature"]);
        repo.write("src/lib.rs", CODE_LIB_RS);
        repo.commit("count things");

        let diff = gitio::review_diff(&repo.path(), "main", 12).expect("diff");
        let files = crate::diffparse::parse(&diff);
        let extracted = units::assemble(
            &repo.path(),
            &files,
            12,
            false,
            true,
            gitio::new_side("main"),
        );
        assert_eq!(extracted.len(), 1, "expected one reviewable file");
        let files = extracted
            .into_iter()
            .map(|(path, units)| ReviewFile::new(path, units))
            .collect();
        app.repo = Some(RepoCtx {
            path: repo.path(),
            name: "test-repo".into(),
            default_branch: "main".into(),
        });
        app.plan = Some(ReviewPlan {
            session_id: 1,
            ref_kind: RefKind::Branch,
            ref_name: "feature".into(),
            base_ref: "main".into(),
            branch_base: "main".into(),
            files,
            file_idx: 0,
            unit_idx: 0,
            decided_total: 0,
            skipped_decided: 0,
        });
        Harness {
            app,
            repo,
            _dir: dir,
        }
    }

    #[test]
    fn a_code_unit_shows_the_scope_and_edits_verbatim() {
        let mut h = code_harness("codeunit");
        h.enter_with(vec![]);
        let unit = h.app.current_unit().expect("a code unit");
        assert!(unit.is_code());
        assert_eq!(
            unit.kind_label(),
            "code · fn main()",
            "the enclosing scope names the unit"
        );
        // Code is edited exactly as it sits in the file — indentation intact.
        assert_eq!(
            h.app.original_display,
            "    let count = 1;\n    print(count);"
        );
    }

    #[test]
    fn a_revision_is_applied_verbatim_with_its_evidence_recorded() {
        let dir = TempDir::new("revise");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: REVISE,
                ..Default::default()
            },
        );
        let mut h = code_harness("revise");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();

        let Some(CandidateState::Ready(s)) = h.app.candidates.first() else {
            panic!("expected a ready candidate");
        };
        assert_eq!(s.action, Action::Rewrite);
        assert_eq!(
            s.evidence.len(),
            1,
            "the model's reading list must survive parsing"
        );
        assert_eq!(s.evidence[0].file, "src/lib.rs");

        h.app.choose_candidate(0);
        assert_eq!(h.app.editor, "    let count = 2;\n    print(count);");
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        let after = h.repo.read("src/lib.rs");
        assert!(after.contains("    let count = 2;"), "{after}");
        assert!(!after.contains("count = 1"), "{after}");
    }

    #[test]
    fn a_failing_check_reverts_the_edit_and_stays_put() {
        let dir = TempDir::new("checkfail");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: REVISE,
                ..Default::default()
            },
        );
        let checker = FakeCli::new(
            &dir,
            "checker",
            FakeCliSpec {
                reply: "error[E0425]: cannot find value",
                exit_code: 1,
                ..Default::default()
            },
        );
        let mut h = code_harness("checkfail");
        h.app.settings.check_command = checker.command();
        let before = h.repo.read("src/lib.rs");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);

        h.app.save_and_continue(&egui::Context::default(), false);
        // The edit must be rolled back, the failure shown, and the review must
        // not advance past a unit whose edit did not survive.
        assert_eq!(
            h.repo.read("src/lib.rs"),
            before,
            "the failing edit must be reverted"
        );
        let err = h
            .app
            .review_error
            .clone()
            .expect("the failure must be surfaced");
        assert!(err.contains("reverted"), "{err}");
        assert!(
            err.contains("cannot find value"),
            "the check's own words: {err}"
        );
        assert_eq!(
            h.app.plan.as_ref().unwrap().decided_total,
            0,
            "must not advance"
        );
        assert_eq!(
            h.app.db.decision_counts(1).0,
            0,
            "no decision recorded for a reverted edit"
        );
        // The check ran where the code lives.
        let seen = std::fs::canonicalize(checker.cwd_seen()).expect("checker cwd");
        assert_eq!(seen, std::fs::canonicalize(h.repo.path()).unwrap());
    }

    #[test]
    fn a_passing_check_lets_the_edit_stand() {
        let dir = TempDir::new("checkok");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: REVISE,
                ..Default::default()
            },
        );
        let checker = FakeCli::new(
            &dir,
            "checker",
            FakeCliSpec {
                reply: "ok",
                ..Default::default()
            },
        );
        let mut h = code_harness("checkok");
        h.app.settings.check_command = checker.command();
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);

        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert!(
            h.repo.read("src/lib.rs").contains("count = 2"),
            "the edit must stand"
        );
        assert_eq!(
            h.app.plan.as_ref().unwrap().decided_total,
            1,
            "and the review moves on"
        );
    }

    #[test]
    fn comment_edits_skip_the_check_unless_asked() {
        let dir = TempDir::new("checkskip");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        // A checker that always fails: if it ran, the edit would be reverted.
        let checker = FakeCli::new(
            &dir,
            "checker",
            FakeCliSpec {
                reply: "boom",
                exit_code: 1,
                ..Default::default()
            },
        );
        let mut h = Harness::new("checkskip");
        h.app.settings.check_command = checker.command();
        h.app.settings.validate_comment_edits = false;
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert!(
            h.repo.read("src/lib.rs").contains("// Counts retries."),
            "edit must stand"
        );

        // Opt comment edits in, and the same failing checker now blocks them.
        h.app.settings.validate_comment_edits = true;
        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h
            .app
            .review_error
            .as_deref()
            .is_some_and(|e| e.contains("reverted")));
    }

    #[test]
    fn a_flag_records_the_concern_without_touching_anything() {
        let dir = TempDir::new("flag");
        let flag_reply = "{\"action\":\"flag\",\
\"justification\":\"races with init — count is read before main runs\"}";
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: flag_reply,
                ..Default::default()
            },
        );
        let mut h = code_harness("flag");
        let before = h.repo.read("src/lib.rs");
        let head = gitio::head_sha(&h.repo.path()).unwrap();
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();

        h.app.choose_candidate(0);
        assert_eq!(
            h.app.editor, h.app.original_display,
            "a flag proposes no text"
        );
        assert_eq!(h.app.current_action(), Action::Flag);

        h.app.save_and_continue(&egui::Context::default(), true);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert_eq!(h.repo.read("src/lib.rs"), before, "a flag must not edit");
        assert_eq!(gitio::head_sha(&h.repo.path()).unwrap(), head, "or commit");
        // But the judgement is on the record, attributed to the model.
        let rows = h.app.db.corpus(10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "flag");
        assert_eq!(rows[0].source, "fake");
    }

    #[test]
    fn editing_after_a_flag_turns_it_into_a_rewrite() {
        let dir = TempDir::new("flagedit");
        let flag_reply = "{\"action\":\"flag\",\"justification\":\"wrong start value\"}";
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: flag_reply,
                ..Default::default()
            },
        );
        let mut h = code_harness("flagedit");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.editor = "    let count = 5;\n    print(count);".into();
        assert_eq!(
            h.app.current_action(),
            Action::Rewrite,
            "a hand edit outranks the flag"
        );
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert!(h.repo.read("src/lib.rs").contains("count = 5"));
    }

    // -- triage order and out-of-order edits --------------------------------

    /// Reviewing a branch the reviewer is not standing on must not move their
    /// checkout: it runs in a worktree of its own, uncommitted work and all,
    /// and the decisions it records still belong to the repository.
    #[test]
    fn reviewing_another_branch_leaves_the_reviewers_checkout_alone() {
        let _root = crate::testkit::WorktreeRoot::new("wt-app");
        let dir = TempDir::new("wt-branch");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let mut app = CraApp::with_db(db);
        app.settings.models.clear();

        let repo = TempRepo::new("wt-branch");
        repo.write(
            "src/lib.rs",
            "fn safe() {
}
",
        );
        repo.commit("base");
        repo.git(&["checkout", "-b", "feature"]);
        repo.write(
            "src/lib.rs",
            "fn safe() {
    // gentle note
}
",
        );
        repo.commit("note");
        repo.git(&["checkout", "main"]);
        // Work in progress that the old in-place checkout refused to move past.
        repo.write(
            "src/scratch.rs",
            "fn half_written() {}
",
        );

        app.repo = Some(RepoCtx {
            path: repo.path(),
            name: "test-repo".into(),
            default_branch: "main".into(),
        });
        app.select_branch("feature");

        assert!(app.ref_error.is_none(), "{:?}", app.ref_error);
        assert!(app.plan.is_some(), "the branch was reviewed all the same");

        // The reviewer's checkout is where they left it, dirty file included.
        assert_eq!(gitio::current_branch(&repo.path()).unwrap(), "main");
        assert_eq!(
            repo.read("src/scratch.rs"),
            "fn half_written() {}
"
        );

        // The review is somewhere else entirely, on the branch it was asked for.
        let work = app.work_dir().expect("a work dir");
        assert_ne!(gitio::path_key(&work), gitio::path_key(&repo.path()));
        assert_eq!(gitio::current_branch(&work).unwrap(), "feature");
        let note = app.ref_note.as_deref().unwrap_or_default();
        assert!(
            note.contains("isolated worktree"),
            "the reviewer is told where: {note}"
        );

        // Identity did not move with it: the session is filed under the repo.
        let plan = app.plan.as_ref().unwrap();
        assert!(
            app.db.decided_units(&repo.path()).is_empty(),
            "nothing decided yet"
        );
        assert!(plan.session_id > 0);

        // Back to the reviewer's own tree, and the worktree is forgotten.
        app.select_working_tree();
        assert_eq!(
            gitio::path_key(&app.work_dir().unwrap()),
            gitio::path_key(&repo.path())
        );
    }

    /// The riskiest-first review visits a late-in-file unit before an earlier
    /// one; both edits still have to be applied exactly where they belong.
    #[test]
    fn triage_reviews_risky_code_first_and_still_applies_edits_correctly() {
        let dir = TempDir::new("triage");
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        let mut app = CraApp::with_db(db);
        app.settings.models.clear();
        assert!(app.settings.triage_order, "riskiest-first is the default");

        let repo = TempRepo::new("triage");
        repo.write("src/lib.rs", "fn safe() {\n}\n\nfn danger() {\n}\n");
        repo.commit("base");
        repo.git(&["checkout", "-b", "feature"]);
        repo.write(
            "src/lib.rs",
            "fn safe() {\n    // gentle note\n}\n\nfn danger() {\n    unsafe { launch(); }\n}\n",
        );
        repo.commit("both fns");
        app.repo = Some(RepoCtx {
            path: repo.path(),
            name: "test-repo".into(),
            default_branch: "main".into(),
        });

        // Through the same entry point the ref picker uses, so the triage
        // ordering in build_plan is what gets exercised.
        app.select_branch("feature");
        let ctx = egui::Context::default();
        app.start_review(&ctx, 0);

        // The unsafe code unit (line 6) outranks the comment (line 2).
        let first = app.current_unit().expect("a unit");
        assert!(first.is_code(), "risky code should lead the review");
        assert_eq!(first.start_line(), 6);
        assert!(crate::triage::assess(&first).score > 30);

        // Grow the code unit by a line, then edit the comment above it.
        app.editor = "    unsafe { launch(); }\n    log();".into();
        app.save_and_continue(&ctx, false);
        assert!(app.review_error.is_none(), "{:?}", app.review_error);

        let second = app.current_unit().expect("the comment unit");
        assert!(!second.is_code());
        assert_eq!(second.start_line(), 2);
        app.editor = "// tightened".into();
        app.save_and_continue(&ctx, false);
        assert!(app.review_error.is_none(), "{:?}", app.review_error);

        // Both edits were applied despite the review running bottom-up.
        assert_eq!(
            repo.read("src/lib.rs"),
            "fn safe() {\n    // tightened\n}\n\nfn danger() {\n    unsafe { launch(); }\n    log();\n}\n"
        );
    }

    // -- whole-branch review ---------------------------------------------------------

    fn wait_for_whole_branch_review(h: &mut Harness) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while h.app.whole_branch_review_running() {
            h.app.pump_messages();
            assert!(
                std::time::Instant::now() < deadline,
                "whole-branch review never came back"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        h.app.pump_messages();
    }

    #[test]
    fn the_whole_branch_review_records_findings_for_human_triage() {
        let dir = TempDir::new("branchpass");
        let reply = "{\"findings\":[\
{\"title\":\"minor duplication\",\"severity\":\"low\",\"detail\":\"copyable\",\"files\":[]},\
{\"title\":\"rename half applied\",\"severity\":\"high\",\
\"detail\":\"old name still read in lib.rs\",\"files\":[\"src/lib.rs\"],\
\"evidence\":[{\"file\":\"src/lib.rs\",\"lines\":\"1-4\",\"note\":\"checked callers\"}]}]}";
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply,
                ..Default::default()
            },
        );
        let mut h = code_harness("branchpass");
        h.app.settings.models = vec![cli.model_config("")];

        h.app.start_whole_branch_review(&egui::Context::default());
        assert!(h.app.whole_branch_review_running());
        wait_for_whole_branch_review(&mut h);

        // The pass ran in the repository, with the branch's diff in the prompt.
        let seen = std::fs::canonicalize(cli.cwd_seen()).expect("cwd");
        assert_eq!(seen, std::fs::canonicalize(h.repo.path()).unwrap());
        let sent = cli.stdin_seen();
        assert!(sent.contains("cross-cutting"), "{sent}");
        assert!(
            sent.contains("let count = 1;"),
            "the diff must travel: {sent}"
        );

        assert!(matches!(
            h.app.whole_branch_review[0],
            WholeBranchReviewState::Done { n: 2, .. }
        ));
        assert_eq!(h.app.findings.len(), 2);
        // Sorted for triage: high first, whatever order the model used.
        assert_eq!(h.app.findings[0].finding.title, "rename half applied");
        assert_eq!(h.app.findings[0].finding.evidence.len(), 1);

        // Dismissal is written through to the record, and the markdown export
        // carries only what is still open.
        let (keep_id, drop_id) = (h.app.findings[0].id, h.app.findings[1].id);
        h.app.dismiss_finding(drop_id);
        assert_eq!(
            h.app.db.finding_status(drop_id).as_deref(),
            Some("dismissed")
        );
        assert_eq!(h.app.db.finding_status(keep_id).as_deref(), Some("open"));
        let md = h.app.findings_markdown();
        assert!(md.contains("rename half applied"), "{md}");
        assert!(!md.contains("minor duplication"), "{md}");
    }

    #[test]
    fn a_stale_branch_reply_from_an_abandoned_plan_is_discarded() {
        let mut h = code_harness("stalebranch");
        h.app.whole_branch_review_seq = 5;
        h.app.whole_branch_review = vec![WholeBranchReviewState::Pending(Default::default())];
        h.app.handle_whole_branch_review(WholeBranchReviewMsg {
            seq: 4,
            model_index: 0,
            model: "fake".into(),
            cancelled: false,
            result: Ok(vec![Finding {
                title: "ghost".into(),
                detail: String::new(),
                severity: "high".into(),
                files: Vec::new(),
                evidence: Vec::new(),
            }]),
            latency_ms: 1,
        });
        assert!(
            h.app.findings.is_empty(),
            "an outdated review must not record findings"
        );
        assert!(
            h.app.whole_branch_review_running(),
            "and must not mark the new review's model complete"
        );
    }

    #[test]
    fn a_recheck_has_no_whole_branch_review() {
        let dir = TempDir::new("norecheck");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("norecheck");
        h.enter_with(vec![cli.model_config("")]);
        h.wait_for_model_replies();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);
        h.app.settings.models.clear();
        h.app.start_recheck(&egui::Context::default(), 10);

        h.app.start_whole_branch_review(&egui::Context::default());
        assert!(
            h.app.whole_branch_review.is_empty(),
            "nothing should launch for a re-check"
        );
    }

    #[test]
    fn a_stale_reply_from_a_previous_comment_is_discarded() {
        let dir = TempDir::new("stale");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec {
                reply: VERDICT,
                ..Default::default()
            },
        );
        let mut h = Harness::new("stale");
        h.enter_with(vec![cli.model_config("")]);

        // The user moves on before the model answers; the late reply carries
        // the old sequence number and must not overwrite the new question.
        let stale_seq = h.app.review_seq;
        h.app.skip_unit(&egui::Context::default());
        assert!(h.app.review_seq > stale_seq);

        h.app.handle_candidate(CandidateMsg {
            seq: stale_seq,
            model_index: 0,
            model: "fake".into(),
            cancelled: false,
            result: Ok(Suggestion {
                action: Action::Delete,
                comment: String::new(),
                justification: "from the previous comment".into(),
                evidence: Vec::new(),
                latency_ms: 1,
            }),
            raw: String::new(),
        });
        assert!(
            !matches!(h.app.candidates[0], CandidateState::Ready(_)),
            "a stale reply was shown against the wrong comment"
        );
    }

    /// Wait for the fix-session reply, or give up.
    fn wait_for_fix_reply(app: &mut CraApp) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while app.fix_running {
            app.pump_messages();
            assert!(
                std::time::Instant::now() < deadline,
                "fix session never came back"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn a_note_parks_the_larger_issue_for_the_followup_screen() {
        let mut h = Harness::new("note_park");
        h.enter_with(vec![]);
        h.app.note_input = "  this counter pattern repeats in four files — extract it  ".into();
        h.app.leave_note();
        assert!(
            h.app.note_input.is_empty(),
            "a saved note leaves the input clear"
        );

        h.app.open_followup();
        assert_eq!(h.app.screen as u8, Screen::Followup as u8);
        assert_eq!(h.app.followup_from as u8, Screen::Review as u8);
        assert_eq!(h.app.notes.len(), 1);
        let n = &h.app.notes[0].note;
        assert_eq!(n.file, "src/lib.rs");
        assert_eq!(
            n.text,
            "this counter pattern repeats in four files — extract it"
        );
        // The note keeps the code the reviewer was looking at, not a pointer
        // into a tree that will have moved on by the fix session.
        assert!(n.excerpt.contains("Increment the counter"), "{}", n.excerpt);
        assert!(
            !h.app.notes[0].checked,
            "notes load unchecked — checking is the human's call"
        );
        // The preamble is offered, not imposed: it appeared in the editable box.
        assert!(h.app.fix_prompt.contains("Resolve the review notes"));
    }

    #[test]
    fn a_blank_note_is_not_saved() {
        let mut h = Harness::new("note_blank");
        h.enter_with(vec![]);
        h.app.note_input = "   ".into();
        h.app.leave_note();
        h.app.open_followup();
        assert!(h.app.notes.is_empty());
    }

    #[test]
    fn a_launch_with_nothing_checked_is_refused() {
        let mut h = Harness::new("note_none");
        h.enter_with(vec![]);
        h.app.note_input = "something".into();
        h.app.leave_note();
        h.app.open_followup();
        h.app.start_fix_session(&egui::Context::default());
        assert!(h.app.fix_error.is_some());
        assert!(!h.app.fix_running);
        assert_eq!(h.app.notes.len(), 1, "a refused launch resolves nothing");
    }

    #[test]
    fn checked_notes_resolve_at_launch_and_dismissed_notes_never_return() {
        let dir = TempDir::new("note_triage");
        let cli = FakeCli::new(
            &dir,
            "fixer",
            FakeCliSpec {
                reply: "all done.",
                ..Default::default()
            },
        );
        let mut h = Harness::new("note_triage");
        h.enter_with(vec![]);
        for text in ["first", "second", "third"] {
            h.app.note_input = text.into();
            h.app.leave_note();
        }
        h.app.open_followup();
        assert_eq!(h.app.notes.len(), 3);

        // Dismissing takes the note off the screen and marks the record.
        let dismissed_id = h.app.notes[2].note.id;
        h.app.dismiss_note(dismissed_id);
        assert_eq!(h.app.db.note_status(dismissed_id), Some("dismissed".into()));
        assert_eq!(h.app.notes.len(), 2);

        // Check the first and launch: it is the session's job now.
        let checked_id = h.app.notes[0].note.id;
        h.app.notes[0].checked = true;
        let mut fixer = cli.model_config("");
        fixer.command = "this-review-command-must-not-run".into();
        h.app.settings.models = vec![fixer];
        h.app.selected_fix_model_index = 0;
        h.app.start_fix_session(&egui::Context::default());
        assert!(h.app.fix_error.is_none());
        assert_eq!(h.app.db.note_status(checked_id), Some("resolved".into()));
        assert_eq!(h.app.notes.len(), 1);
        assert_eq!(h.app.notes[0].note.text, "second");

        // The opening prompt carried the checked note — and only it.
        assert_eq!(h.app.fix_convo.len(), 1);
        assert!(h.app.fix_convo[0].prompt.contains("first"));
        assert!(!h.app.fix_convo[0].prompt.contains("second"));
        wait_for_fix_reply(&mut h.app);
        assert_eq!(h.app.fix_convo[0].reply, "all done.");
        assert!(
            cli.stdin_seen().contains("first"),
            "the prompt travels on stdin"
        );

        // Next visit: dismissed and resolved stay gone, unchecked waits.
        h.app.open_followup();
        assert_eq!(h.app.notes.len(), 1);
        assert_eq!(h.app.notes[0].note.text, "second");
    }

    #[test]
    fn notes_are_scoped_to_their_repository() {
        let mut h = Harness::new("note_scope");
        h.enter_with(vec![]);
        h.app.note_input = "ours".into();
        h.app.leave_note();
        h.app
            .db
            .log_note(0, "some/other/checkout", "src/x.rs", 1, 1, "", "theirs");
        h.app.open_followup();
        assert_eq!(h.app.notes.len(), 1);
        assert_eq!(h.app.notes[0].note.text, "ours");
    }

    // -- publishing -----------------------------------------------------------

    /// Give the harness repository a bare remote with `feature` already on it,
    /// so the branch under review has somewhere to go and a position to be
    /// compared against. Returns the remote, which must be held for as long as
    /// the test needs it.
    fn publishable(h: &Harness) -> TempDir {
        let remote = TempDir::new("harness-remote");
        let path = remote.path().to_string_lossy().replace('\\', "/");
        gitio::run(&h.repo.path(), "git", &["init", "--bare", &path]).expect("bare remote");
        h.repo.git(&["remote", "add", "origin", &path]);
        h.repo.git(&["push", "--set-upstream", "origin", "feature"]);
        remote
    }

    /// A commit of this session's, as the database records one: what proves a
    /// branch holds nothing but the review's own work.
    fn record_commit(app: &mut CraApp, session_id: i64, sha: &str) {
        let id = app.db.log_decision(&crate::db::DecisionRecord {
            session_id,
            file: "src/lib.rs",
            line_start: 1,
            line_end: 1,
            original: "before",
            action: "rewrite",
            final_text: "after",
            source: "human",
            human_edited: true,
            committed: true,
            commit_sha: Some(sha),
            justification: None,
            unit_json: None,
            blinded: false,
        });
        app.db.mark_committed(id, sha);
    }

    /// Landing on the summary must already know where the commits stand — the
    /// buttons that publish them are drawn from it on the very first frame.
    #[test]
    fn arriving_at_the_summary_reads_where_the_review_commits_stand() {
        let mut h = Harness::new("pub-summary");
        let _remote = publishable(&h);
        h.repo.write("src/lib.rs", "fn main() { /* fixed */ }\n");
        h.repo.commit("review: fix the comment");

        h.app.goto(Screen::Summary);
        let d = h
            .app
            .delivery
            .as_ref()
            .expect("the delivery state is read on arrival");
        assert_eq!(d.branch.as_deref(), Some("feature"));
        assert_eq!(d.remote.as_deref(), Some("origin"));
        assert_eq!(d.upstream.as_deref(), Some("origin/feature"));
        assert_eq!(d.ahead(), 1, "the fix commit, and only it");
        assert!(d.can_push());

        // The stacked pull request is ready to send without being filled in.
        assert_eq!(h.app.stack.branch, "review/feature-fixes");
        assert!(
            h.app.stack.title.contains("feature"),
            "{}",
            h.app.stack.title
        );
        assert!(h.app.stack.body.contains("feature"), "{}", h.app.stack.body);
        assert_eq!(
            h.app.stack_base(),
            "feature",
            "a stack sits on the reviewed branch"
        );
    }

    /// A branch nobody has pushed cannot be stacked *on*, so the pull request
    /// targets what the review was actually run against instead of naming a
    /// base the server has never heard of.
    #[test]
    fn an_unpushed_branch_stacks_onto_the_review_base_instead() {
        let mut h = Harness::new("pub-unpushed");
        h.app.goto(Screen::Summary);
        let d = h.app.delivery.as_ref().expect("state");
        assert!(d.upstream.is_none(), "nothing was ever pushed");
        assert!(!d.can_push(), "and there is no remote to push to");
        assert_eq!(h.app.stack_base(), "main");
    }

    /// The restore offer is the difference between a stack and a mess, and it
    /// is only safe when every commit it would rewind is the review's own.
    #[test]
    fn restoring_is_offered_only_for_commits_the_review_itself_made() {
        let mut h = Harness::new("pub-restore-offer");
        let _remote = publishable(&h);
        h.repo.write("src/lib.rs", "fn main() { /* fixed */ }\n");
        h.repo.commit("review: fix the comment");
        let sha = h.repo.git(&["rev-parse", "HEAD"]).trim().to_string();

        h.app.goto(Screen::Summary);
        let why = h
            .app
            .restore_blocker()
            .expect("an unrecognised commit blocks it");
        assert!(why.contains("not made by this review"), "{why}");

        // The same commit, now on the record as this session's.
        let session_id = h.app.plan.as_ref().unwrap().session_id;
        record_commit(&mut h.app, session_id, &sha);
        assert_eq!(h.app.db.session_commits(session_id), vec![sha]);
        assert!(
            h.app.restore_blocker().is_none(),
            "the branch holds only our work"
        );
    }

    /// Publishing is about commits, and a re-check makes none — it re-judges
    /// history without touching the working tree.
    #[test]
    fn a_recheck_has_nothing_to_publish() {
        let mut h = Harness::new("pub-recheck");
        let _remote = publishable(&h);
        h.app.plan.as_mut().unwrap().ref_kind = RefKind::Recheck;
        h.app.goto(Screen::Summary);
        assert!(
            h.app.delivery.is_none(),
            "no route out of a review that wrote nothing"
        );
    }

    /// The whole way through, in the app's own terms: a fix commit in the
    /// review's checkout, `P` on the summary, and the remote branch has it.
    #[test]
    fn pushing_from_the_summary_puts_the_fixes_on_the_remote_branch() {
        let mut h = Harness::new("pub-push-app");
        let remote = publishable(&h);
        h.repo.write("src/lib.rs", "fn main() { /* fixed */ }\n");
        h.repo.commit("review: fix the comment");
        let sha = h.repo.git(&["rev-parse", "HEAD"]).trim().to_string();

        h.app.goto(Screen::Summary);
        h.app.start_push();
        assert!(h.app.publish.running(), "the push runs off the UI thread");
        wait_for_publish(&mut h.app);

        match &h.app.publish {
            PublishState::Done(o) => {
                assert!(o.headline.contains("origin/feature"), "{}", o.headline)
            }
            PublishState::Failed(e) => panic!("push failed: {e}"),
            _ => panic!("the push never reported back"),
        }
        let on_remote = gitio::run(
            &remote.path().to_string_lossy(),
            "git",
            &["rev-parse", "refs/heads/feature"],
        )
        .expect("remote branch");
        assert_eq!(on_remote.trim(), sha);
        // And the screen now describes the state the push left behind.
        assert_eq!(h.app.delivery.as_ref().unwrap().ahead(), 0);
    }

    /// The other way out, end to end: the fixes go on a branch of their own,
    /// `gh` is asked for a pull request into the branch that was reviewed, and
    /// that branch is put back where the remote has it so the pull request is
    /// the only place the fixes live.
    #[test]
    fn stacking_from_the_summary_proposes_the_fixes_back_to_the_reviewed_branch() {
        let mut h = Harness::new("pub-stack-app");
        let remote = publishable(&h);
        let before = h
            .repo
            .git(&["rev-parse", "origin/feature"])
            .trim()
            .to_string();
        h.repo.write("src/lib.rs", "fn main() { /* fixed */ }\n");
        h.repo.commit("review: fix the comment");
        let sha = h.repo.git(&["rev-parse", "HEAD"]).trim().to_string();

        let bin = TempDir::new("pub-stack-app-gh");
        let gh = FakeCli::new(
            &bin,
            "gh",
            FakeCliSpec {
                reply: "https://github.test/o/r/pull/12\n",
                ..Default::default()
            },
        );
        h.app.settings.gh_path = gh.command();

        h.app.goto(Screen::Summary);
        let session_id = h.app.plan.as_ref().unwrap().session_id;
        record_commit(&mut h.app, session_id, &sha);
        assert!(
            h.app.restore_blocker().is_none(),
            "every unpushed commit is ours"
        );
        h.app.stack.restore = true;
        h.app.start_stacked_pr();
        wait_for_publish(&mut h.app);

        match &h.app.publish {
            PublishState::Done(o) => {
                assert_eq!(o.url.as_deref(), Some("https://github.test/o/r/pull/12"))
            }
            PublishState::Failed(e) => panic!("stacking failed: {e}"),
            _ => panic!("the publish never reported back"),
        }
        let argv = gh.argv_seen();
        assert!(
            argv.contains("--base feature"),
            "it stacks onto the reviewed branch: {argv}"
        );
        assert!(argv.contains("--head review/feature-fixes"), "{argv}");

        let remote_dir = remote.path().to_string_lossy().to_string();
        let stacked = gitio::run(
            &remote_dir,
            "git",
            &["rev-parse", "refs/heads/review/feature-fixes"],
        )
        .expect("the fixes branch is on the remote");
        assert_eq!(stacked.trim(), sha);
        assert_eq!(
            gitio::run(&remote_dir, "git", &["rev-parse", "refs/heads/feature"])
                .unwrap()
                .trim(),
            before,
            "the reviewed branch on the remote was left alone"
        );
        assert_eq!(
            h.repo.git(&["rev-parse", "feature"]).trim(),
            before,
            "and locally it is back where the remote has it"
        );
    }

    /// A ref with every unit already decided used to dead-end at the picker
    /// with an error. That is the state a *finished* review leaves behind, and
    /// its fix commits may still be sitting in the worktree with nowhere to
    /// go — so it opens the summary instead, on the session that made them.
    #[test]
    fn a_fully_decided_ref_reopens_on_the_summary_so_its_commits_can_be_delivered() {
        let mut h = Harness::new("pub-reopen");
        let _remote = publishable(&h);
        replan(&mut h);
        let first_session = h.app.plan.as_ref().unwrap().session_id;

        // Decide everything the plan offers.
        h.enter_with(vec![]);
        for _ in 0..2 {
            h.app.save_and_continue(&egui::Context::default(), false);
        }
        // And a fix commit, as a real review leaves behind. It touches a file
        // of its own: rewriting src/lib.rs would take the reviewed comments
        // out of the diff, which is a different reason for an empty plan.
        h.repo.write("src/extra.rs", "fn extra() {}\n");
        h.repo.commit("review: add the missing helper");
        let sha = h.repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        record_commit(&mut h.app, first_session, &sha);

        // Selecting it again: nothing left to judge, everything left to send.
        replan(&mut h);
        assert!(
            h.app.ref_error.is_none(),
            "not an error: {:?}",
            h.app.ref_error
        );
        assert_eq!(
            h.app.screen as u8,
            Screen::Summary as u8,
            "it opens where publishing lives"
        );

        let plan = h.app.plan.as_ref().expect("a plan carries the session");
        assert!(plan.nothing_left());
        assert_eq!(
            plan.session_id, first_session,
            "the session its commits were made in"
        );
        assert_eq!(plan.reported_units(), 2, "'2 / 0' would read as a bug");

        // And DELIVER is live, with the commit it is there for.
        let d = h
            .app
            .delivery
            .as_ref()
            .expect("the delivery state is read on arrival");
        assert_eq!(
            d.unpushed,
            vec![sha],
            "the fix commit is what there is to publish"
        );
        assert!(d.can_push());
        assert!(
            h.app.restore_blocker().is_none(),
            "reopening the session proves it is ours"
        );
    }

    /// Rescuing a detached review's commits onto a branch is only half a
    /// rescue if that branch cannot then be delivered. Its name belongs to no
    /// session, so the session has to be found by the commits themselves.
    #[test]
    fn a_branch_rescued_from_a_detached_review_can_still_be_delivered() {
        let mut h = Harness::new("pub-rescued");
        let _remote = publishable(&h);
        replan(&mut h);
        let session = h.app.plan.as_ref().unwrap().session_id;
        h.enter_with(vec![]);
        for _ in 0..2 {
            h.app.save_and_continue(&egui::Context::default(), false);
        }
        h.repo.write("src/extra.rs", "fn extra() {}\n");
        h.repo.commit("review: add the missing helper");
        let sha = h.repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        record_commit(&mut h.app, session, &sha);

        // The shape `worktree::park_stranded` leaves behind: the commits on a
        // branch whose name no session was ever opened under.
        let rescued = crate::publish::suggested_branch("feature");
        h.repo.git(&["branch", &rescued]);
        assert!(
            h.app.db.last_session(&h.repo.path(), &rescued).is_none(),
            "no session by name"
        );

        h.app.settings.review_code = false;
        h.app
            .build_plan(RefKind::Branch, rescued.clone(), "main".into());
        assert!(
            h.app.ref_error.is_none(),
            "not an error: {:?}",
            h.app.ref_error
        );
        assert_eq!(h.app.screen as u8, Screen::Summary as u8);
        assert_eq!(
            h.app.plan.as_ref().unwrap().session_id,
            session,
            "found by the commit, since the name could not say"
        );
        assert!(h.app.delivery.as_ref().is_some_and(|d| d.can_push()));
        // Nothing derivable says what a rescued branch should target, so the
        // base is the reviewer's to set — which is why the field is editable.
        assert!(
            !h.app.stack.base.is_empty(),
            "a base is offered to start from"
        );
    }

    fn wait_for_publish(app: &mut CraApp) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while app.publish.running() {
            app.pump_messages();
            assert!(
                std::time::Instant::now() < deadline,
                "the publish never reported back"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod discovery_state_tests {
    use super::*;
    use crate::discover::DiscoveredRepo;
    use crate::testkit::TempDir;

    fn app(tag: &str) -> (TempDir, CraApp) {
        let dir = TempDir::new(tag);
        let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
        (dir, CraApp::with_db(db))
    }

    fn found(name: &str, path: Option<&str>, slug: Option<&str>, ts: i64) -> DiscoveredRepo {
        DiscoveredRepo {
            name: name.into(),
            path: path.map(Into::into),
            slug: slug.map(Into::into),
            last_update: ts,
        }
    }

    #[test]
    fn a_scan_streams_in_merges_sources_and_prunes_on_completion() {
        let (_dir, mut app) = app("disc-state");
        // a cached entry the new scan will not find again
        app.discovered = vec![found("gone", Some("/home/e/gone"), None, 10)];
        app.repo_scan_seq = 1;
        app.scanning_local = true;
        app.scanning_gh = true;

        app.handle_repo(RepoMsg::Found {
            seq: 1,
            repo: found("proj", Some("/home/e/proj"), Some("e/proj"), 100),
        });
        // a streamed find is visible immediately, next to the cached entry
        assert_eq!(app.discovered.len(), 2);

        app.handle_repo(RepoMsg::Found {
            seq: 1,
            repo: found("proj", None, Some("e/proj"), 900),
        });
        app.handle_repo(RepoMsg::Done {
            seq: 1,
            source: RepoSource::Local,
            err: None,
        });
        assert!(app.scanning_gh, "one Done must not stop the other source");
        app.handle_repo(RepoMsg::Done {
            seq: 1,
            source: RepoSource::GitHub,
            err: None,
        });

        // completion replaced the merged view: the vanished repo dropped out,
        // and the two sightings of proj are one row carrying the newest
        // activity either side saw
        assert_eq!(app.discovered.len(), 1);
        assert_eq!(app.discovered[0].last_update, 900);
        assert_eq!(app.discovered[0].path.as_deref(), Some("/home/e/proj"));
        // and the merged list went to the cache another session will load
        let cache = crate::discover::load_cache(&app.db);
        assert_eq!(cache.repos.len(), 1);
        assert!(cache.fetched_at > 0);
    }

    #[test]
    fn a_late_find_from_an_abandoned_scan_is_dropped() {
        let (_dir, mut app) = app("disc-stale");
        app.repo_scan_seq = 2;
        app.handle_repo(RepoMsg::Found {
            seq: 1,
            repo: found("old", Some("/x"), None, 1),
        });
        assert!(app.discovered.is_empty());
    }
}
