//! Top-level egui application: state, screen routing, hotkeys, async plumbing.

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};

use crate::comments::{self, CommentUnit};
use crate::db::Db;
use crate::gitio::{self, BranchInfo, PrInfo};
use crate::models::{self, Action, CandidateMsg, Suggestion, Turn};
use crate::review::{self, Choice, RefKind, ReviewFile, ReviewPlan};
use crate::settings::Settings;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    RepoPicker,
    RefPicker,
    FilePicker,
    Review,
    Summary,
    Settings,
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
    Pending,
    Ready(Suggestion),
    Failed(String),
}

pub enum Msg {
    Prs(Result<Vec<PrInfo>, String>),
    Cand(CandidateMsg),
}

pub struct CraApp {
    pub db: Db,
    pub settings: Settings,
    pub screen: Screen,
    pub prev_screen: Screen,

    pub log_lines: VecDeque<(String, String)>,
    pub status: String,

    // repo picker
    pub repo_input: String,
    pub repo_sel: usize,
    pub repo_error: Option<String>,
    pub repo: Option<RepoCtx>,

    // ref picker
    pub ref_tab: RefTab,
    pub branches: Vec<BranchInfo>,
    pub prs: Vec<PrInfo>,
    pub prs_loading: bool,
    pub prs_error: Option<String>,
    pub ref_sel: usize,
    pub ref_error: Option<String>,

    // file picker
    pub plan: Option<ReviewPlan>,
    pub file_sel: usize,

    // review screen
    pub review_seq: u64,
    pub candidates: Vec<CandidateState>,
    pub chosen: Option<Choice>,
    pub editor: String,
    pub candidate_baseline: Option<String>,
    /// The comment exactly as it sits on disk, indentation included.
    pub original_text: String,
    /// `original_text` dedented — the baseline the editor is compared against,
    /// since the editor works in dedented space.
    pub original_display: String,
    pub review_error: Option<String>,
    pub focus_editor: bool,

    /// Per-slot conversation ID; `None` means the slot has no usable session yet.
    pub sessions: Vec<Option<String>>,
    /// Per-slot record of sent and received turns.
    pub convos: Vec<Vec<Turn>>,
    pub follow_up: String,
    pub show_prompt: Option<usize>,
    pub focus_follow_up: bool,

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
        CraApp {
            db,
            settings,
            screen: Screen::RepoPicker,
            prev_screen: Screen::RepoPicker,
            log_lines: VecDeque::new(),
            status: "pick a repository".into(),
            repo_input: String::new(),
            repo_sel: 0,
            repo_error: None,
            repo: None,
            ref_tab: RefTab::Branches,
            branches: Vec::new(),
            prs: Vec::new(),
            prs_loading: false,
            prs_error: None,
            ref_sel: 0,
            ref_error: None,
            plan: None,
            file_sel: 0,
            review_seq: 0,
            candidates: Vec::new(),
            chosen: None,
            editor: String::new(),
            candidate_baseline: None,
            original_text: String::new(),
            original_display: String::new(),
            review_error: None,
            focus_editor: false,
            sessions: Vec::new(),
            convos: Vec::new(),
            follow_up: String::new(),
            show_prompt: None,
            focus_follow_up: false,
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
        self.note("repo", &format!("selected {path}"));
        self.load_refs();
        self.ref_sel = 0;
        self.ref_tab = RefTab::Branches;
        self.screen = Screen::RefPicker;
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

    pub fn select_branch(&mut self, branch: &str) {
        let Some(repo) = &self.repo else { return };
        let path = repo.path.clone();
        let default = repo.default_branch.clone();
        let cur = gitio::current_branch(&path).unwrap_or_default();
        if cur != branch {
            if gitio::is_dirty(&path) {
                self.ref_error = Some(format!(
                    "working tree has uncommitted changes; commit/stash before switching to {branch}"
                ));
                return;
            }
            if let Err(e) = gitio::checkout(&path, branch) {
                self.ref_error = Some(e);
                return;
            }
        }
        let base = if branch != default {
            default
        } else if gitio::is_dirty(&path) {
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
        let name = self
            .repo
            .as_ref()
            .and_then(|r| gitio::current_branch(&r.path).ok())
            .unwrap_or_else(|| "HEAD".into());
        self.build_plan(RefKind::WorkingTree, name, String::new());
    }

    pub fn select_pr(&mut self, pr: &PrInfo) {
        let Some(repo) = &self.repo else { return };
        let path = repo.path.clone();
        if gitio::is_dirty(&path) {
            self.ref_error = Some(
                "working tree has uncommitted changes; commit/stash before checking out a PR".into(),
            );
            return;
        }
        let gh = self.settings.gh_path.clone();
        if let Err(e) = gitio::pr_checkout(&path, &gh, pr.number) {
            self.ref_error = Some(e);
            return;
        }
        self.build_plan(RefKind::Pr(pr.number), pr.head_ref.clone(), pr.base_ref.clone());
    }

    fn build_plan(&mut self, kind: RefKind, ref_name: String, base: String) {
        let Some(repo) = &self.repo else { return };
        let path = repo.path.clone();
        let diff = match gitio::review_diff(&path, &base, self.settings.context_lines) {
            Ok(d) => d,
            Err(e) => {
                self.ref_error = Some(e);
                return;
            }
        };
        let files = crate::diffparse::parse(&diff);
        let extracted = comments::extract_units(&files, self.settings.context_lines);
        if extracted.is_empty() {
            self.ref_error = Some(format!(
                "no reviewable comment hunks in {} (base: {})",
                ref_name,
                gitio::base_label(&base)
            ));
            return;
        }
        let session_id =
            self.db
                .new_session(&path, &kind.label(), &ref_name, gitio::base_label(&base));
        let n_units: usize = extracted.iter().map(|(_, u)| u.len()).sum();
        let review_files = extracted
            .into_iter()
            .map(|(path, units)| ReviewFile { path, units, line_offset: 0, decided: 0 })
            .collect::<Vec<_>>();
        self.note(
            "session",
            &format!("#{session_id} {} — {} comments in {} files", ref_name, n_units, review_files.len()),
        );
        self.plan = Some(ReviewPlan {
            session_id,
            ref_kind: kind,
            ref_name,
            base_ref: gitio::base_label(&base).to_string(),
            files: review_files,
            file_idx: 0,
            unit_idx: 0,
            decided_total: 0,
        });
        self.ref_error = None;
        self.file_sel = 0;
        self.screen = Screen::FilePicker;
    }

    // -- review -------------------------------------------------------------

    pub fn start_review(&mut self, ctx: &egui::Context, file_idx: usize) {
        if let Some(plan) = &mut self.plan {
            plan.jump_to_file(file_idx);
            // Skip empty files (shouldn't happen; files always have units).
            if plan.current().is_none() {
                self.screen = Screen::Summary;
                return;
            }
        }
        self.screen = Screen::Review;
        self.enter_unit(ctx);
    }

    /// Start re-judging comments that were decided before. Comparing a
    /// reviewer against their own earlier self is the only way to know how
    /// much of a model's disagreement is noise rather than error — without it
    /// an agreement score has no scale to be read against.
    pub fn start_recheck(&mut self, ctx: &egui::Context, limit: usize) {
        let corpus = crate::eval::Corpus::from_db(&self.db, limit);
        if corpus.entries.is_empty() {
            self.ref_error = Some(
                "no past decisions to re-check yet — review some comments first".into(),
            );
            return;
        }
        // Group by file so the plan looks like any other review.
        let mut files: Vec<ReviewFile> = Vec::new();
        for entry in corpus.entries {
            match files.iter_mut().find(|f| f.path == entry.unit.file) {
                Some(f) => f.units.push(entry.unit),
                None => files.push(ReviewFile {
                    path: entry.unit.file.clone(),
                    units: vec![entry.unit],
                    line_offset: 0,
                    decided: 0,
                }),
            }
        }
        let n_units: usize = files.iter().map(|f| f.units.len()).sum();
        let session_id = self.db.new_session(
            self.repo.as_ref().map(|r| r.path.as_str()).unwrap_or(""),
            "re-check",
            "past decisions",
            "n/a",
        );
        self.note("session", &format!("#{session_id} re-check — {n_units} past comment(s)"));
        self.plan = Some(ReviewPlan {
            session_id,
            ref_kind: RefKind::Recheck,
            ref_name: "past decisions".into(),
            base_ref: "n/a".into(),
            files,
            file_idx: 0,
            unit_idx: 0,
            decided_total: 0,
        });
        self.ref_error = None;
        self.file_sel = 0;
        self.start_review(ctx, 0);
    }

    /// Prepare state for the current unit and fire the model CLIs.
    pub fn enter_unit(&mut self, ctx: &egui::Context) {
        self.review_seq += 1;
        self.chosen = None;
        self.candidate_baseline = None;
        self.review_error = None;

        let Some((unit, file, line)) = self.plan.as_ref().and_then(|p| {
            p.current().map(|(_, u)| (u.clone(), u.file.clone(), u.start_line))
        }) else {
            self.screen = Screen::Summary;
            return;
        };
        self.original_text = unit.raw_lines.join("\n");
        // The editor shows the comment flush left; the unit's indentation is
        // put back by `reindent` when the edit is written to the file.
        self.original_display = unit.dedent(&self.original_text);
        self.editor = self.original_display.clone();

        let prompt = comments::build_prompt(&unit);
        let timeout = self.settings.model_timeout_secs;
        self.candidates = self
            .settings
            .models
            .iter()
            .map(|m| if m.enabled { CandidateState::Pending } else { CandidateState::Disabled })
            .collect();
        self.convos = vec![Vec::new(); self.settings.models.len()];
        self.sessions = vec![None; self.settings.models.len()];
        self.follow_up.clear();
        self.show_prompt = None;
        for (idx, slot) in self.settings.enabled_models() {
            // A slot that names no session key takes an id of our choosing;
            // the rest report theirs in the reply and we pick it up there.
            let command = if slot.session_key.trim().is_empty() && slot.command.contains("{session}")
            {
                let id = uuid::Uuid::new_v4().to_string();
                let command = slot.command.replace("{session}", &id);
                self.sessions[idx] = Some(id);
                command
            } else {
                slot.command.clone()
            };
            self.convos[idx].push(Turn { prompt: prompt.clone(), reply: String::new() });
            let tx = self.tx.clone();
            models::spawn_model(
                self.review_seq,
                idx,
                slot,
                command,
                prompt.clone(),
                timeout,
                move |m| {
                    let _ = tx.send(Msg::Cand(m));
                },
                ctx.clone(),
            );
        }
        self.note("review", &format!("{file}:{line} — querying models"));
    }

    /// A slot can take a follow-up once its previous request has come back and
    /// it has a session to resume. Waiting for the reply keeps one answer per
    /// request, so a late one can never be misfiled against the wrong turn.
    pub fn can_ask(&self, slot_idx: usize) -> bool {
        let settled = matches!(
            self.candidates.get(slot_idx),
            Some(CandidateState::Ready(_)) | Some(CandidateState::Failed(_))
        );
        let resumable = self
            .settings
            .models
            .get(slot_idx)
            .is_some_and(|m| !m.resume_command.trim().is_empty());
        settled && resumable && self.sessions.get(slot_idx).is_some_and(|s| s.is_some())
    }

    /// Send the pending follow-up to one slot, or to every slot that can take
    /// it. Each goes out on the CLI's own resumed session, so only the new
    /// message travels — the model still has the rest of the conversation.
    pub fn ask_followup(&mut self, ctx: &egui::Context, slot: Option<usize>) {
        let message = self.follow_up.trim().to_string();
        if message.is_empty() {
            return;
        }
        let targets: Vec<usize> = match slot {
            Some(i) => vec![i],
            None => (0..self.candidates.len()).collect(),
        };
        let timeout = self.settings.model_timeout_secs;
        let mut sent = Vec::new();
        for idx in targets {
            if !self.can_ask(idx) {
                continue;
            }
            let Some(slot_cfg) = self.settings.models.get(idx).cloned() else { continue };
            let Some(session) = self.sessions[idx].clone() else { continue };
            let command = slot_cfg.resume_command.replace("{session}", &session);
            let prompt = models::followup_prompt(&message);
            self.convos[idx].push(Turn { prompt: prompt.clone(), reply: String::new() });
            self.candidates[idx] = CandidateState::Pending;
            let tx = self.tx.clone();
            models::spawn_model(
                self.review_seq,
                idx,
                slot_cfg.clone(),
                command,
                prompt,
                timeout,
                move |m| {
                    let _ = tx.send(Msg::Cand(m));
                },
                ctx.clone(),
            );
            sent.push(slot_cfg.name);
        }
        if sent.is_empty() {
            self.review_error =
                Some("no model has a resumable session ready for a follow-up yet".into());
            return;
        }
        self.follow_up.clear();
        self.note("follow-up", &format!("asked {}: {}", sent.join(", "), truncate(&message, 80)));
    }

    /// Display position -> slot index for the current comment. Identity when
    /// blinding is off; a stable shuffle when it is on.
    pub fn candidate_order(&self) -> Vec<usize> {
        let n = self.candidates.len();
        if !self.settings.blind_review {
            return (0..n).collect();
        }
        match self.current_unit() {
            Some(u) => review::blind_order(review::unit_seed(&u.file, u.start_line), n),
            None => (0..n).collect(),
        }
    }

    /// Whether model identities are currently hidden. Blinding lifts once a
    /// choice is made, so the provenance being recorded is still visible.
    pub fn names_hidden(&self) -> bool {
        self.settings.blind_review && self.chosen.is_none()
    }

    /// What to call slot `idx` at display position `pos` right now.
    pub fn slot_label(&self, idx: usize, pos: usize) -> String {
        if self.names_hidden() {
            format!("model {}", (b'A' + pos as u8) as char)
        } else {
            self.settings
                .models
                .get(idx)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| format!("model {idx}"))
        }
    }

    pub fn current_unit(&self) -> Option<CommentUnit> {
        self.plan.as_ref().and_then(|p| p.current().map(|(_, u)| u.clone()))
    }

    pub fn choose_candidate(&mut self, slot_idx: usize) {
        let Some(CandidateState::Ready(s)) = self.candidates.get(slot_idx) else { return };
        let s = s.clone();
        let Some(unit) = self.current_unit() else { return };
        let text = match s.action {
            Action::Keep => self.original_display.clone(),
            Action::Delete => String::new(),
            Action::Rewrite => unit.dedent(&unit.format_replacement(&s.comment).join("\n")),
        };
        self.editor = text.clone();
        self.candidate_baseline = Some(text);
        self.chosen = Some(Choice::Candidate(slot_idx));
        let name = self.settings.models[slot_idx].name.clone();
        self.note("choice", &format!("picked {} ({})", name, s.action.label()));
    }

    pub fn choose_keep(&mut self) {
        self.editor = self.original_display.clone();
        self.candidate_baseline = None;
        self.chosen = Some(Choice::KeepOriginal);
        self.note("choice", "keep original");
    }

    pub fn choose_delete(&mut self) {
        self.editor.clear();
        self.candidate_baseline = None;
        self.chosen = Some(Choice::Delete);
        self.note("choice", "delete comment");
    }

    /// Shared save/commit path. Applies the editor content to the working
    /// tree, logs the decision, optionally commits, then advances.
    pub fn save_and_continue(&mut self, ctx: &egui::Context, commit: bool) {
        let Some(unit) = self.current_unit() else { return };
        let Some(repo_path) = self.repo.as_ref().map(|r| r.path.clone()) else { return };

        let action = review::final_action(&self.editor, &self.original_display);
        let chosen_model = match &self.chosen {
            Some(Choice::Candidate(i)) => {
                self.settings.models.get(*i).map(|m| (m.name.clone(), m.coauthor.clone()))
            }
            _ => None,
        };
        let provenance = review::derive_provenance(
            &self.chosen,
            chosen_model.as_ref().map(|(n, c)| (n.as_str(), c.as_str())),
            &self.editor,
            self.candidate_baseline.as_deref(),
            &self.original_display,
        );
        let justification = match &self.chosen {
            Some(Choice::Candidate(i)) => match self.candidates.get(*i) {
                Some(CandidateState::Ready(s)) => Some(s.justification.clone()),
                _ => None,
            },
            _ => None,
        };

        // Apply to the working tree when the text changed. A re-check is
        // measuring the reviewer, not editing code, so it stops short of this.
        let recheck = self.plan.as_ref().is_some_and(|p| p.is_recheck());
        let new_lines = unit.reindent(&self.editor);
        let final_text = new_lines.join("\n");
        let mut delta = 0i64;
        if action != Action::Keep && !recheck {
            let Some(plan) = &self.plan else { return };
            let file = &plan.files[plan.file_idx];
            match review::apply_edit(&repo_path, file, &unit, &new_lines) {
                Ok(d) => delta = d,
                Err(e) => {
                    self.review_error = Some(e.clone());
                    self.note("error", &e);
                    return;
                }
            }
        }

        // Commit if asked and there is something to commit.
        let mut sha = None;
        let mut committed = false;
        if commit && !recheck {
            if action == Action::Keep {
                self.note("commit", "kept original — nothing to commit");
            } else {
                let msg = review::commit_message(&unit, action, &provenance, justification.as_deref());
                match gitio::stage_and_commit(&repo_path, &unit.file, &msg) {
                    Ok(s) => {
                        committed = true;
                        self.note("commit", &format!("{} {}", &s[..8.min(s.len())], unit.file));
                        sha = Some(s);
                    }
                    Err(e) => {
                        self.review_error = Some(e.clone());
                        self.note("error", &e);
                        return;
                    }
                }
            }
        }

        let session_id = self.plan.as_ref().map(|p| p.session_id).unwrap_or(0);
        // Store the unit itself so this judgement can be replayed against a
        // different model later without needing the repository to still exist.
        let unit_json = serde_json::to_string(&unit).ok();
        self.db.log_decision(&crate::db::DecisionRecord {
            session_id,
            file: &unit.file,
            line_start: unit.start_line,
            line_end: unit.end_line,
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
        self.note(
            "decision",
            &format!("{} {}:{} ({})", action.as_str(), unit.file, unit.start_line, provenance.source_str()),
        );

        if let Some(plan) = &mut self.plan {
            plan.files[plan.file_idx].line_offset += delta;
            plan.files[plan.file_idx].decided += 1;
            plan.decided_total += 1;
            if plan.advance() {
                self.enter_unit(ctx);
            } else {
                self.screen = Screen::Summary;
                self.note("session", "review complete");
            }
        }
    }

    pub fn skip_unit(&mut self, ctx: &egui::Context) {
        if let Some(unit) = self.current_unit() {
            self.note("skip", &format!("{}:{}", unit.file, unit.start_line));
        }
        if let Some(plan) = &mut self.plan {
            if plan.advance() {
                self.enter_unit(ctx);
            } else {
                self.screen = Screen::Summary;
            }
        }
    }

    pub fn prev_unit(&mut self, ctx: &egui::Context) {
        if let Some(plan) = &mut self.plan {
            if plan.retreat() {
                self.enter_unit(ctx);
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
                Msg::Cand(c) => self.handle_candidate(c),
            }
        }
    }

    fn handle_candidate(&mut self, c: CandidateMsg) {
        if c.seq != self.review_seq {
            self.db.log("stale", &format!("discarded late reply from {}", c.model));
            return;
        }
        let (file, start, end) = self
            .current_unit()
            .map(|u| (u.file, u.start_line, u.end_line))
            .unwrap_or_default();
        let session_id = self.plan.as_ref().map(|p| p.session_id).unwrap_or(0);
        // Track the CLI's own id so the next turn resumes this conversation.
        // Take the newest one each time: a CLI is free to hand back a fresh id
        // when it resumes, and following it keeps the chain unbroken.
        if let Some(key) = self.settings.models.get(c.slot_idx).map(|m| m.session_key.clone()) {
            if let Some(id) = models::extract_session_id(&c.raw, &key) {
                if let Some(slot) = self.sessions.get_mut(c.slot_idx) {
                    *slot = Some(id);
                }
            }
        }
        if let Some(turn) = self.convos.get_mut(c.slot_idx).and_then(|t| t.last_mut()) {
            turn.reply = if c.raw.trim().is_empty() {
                match &c.result {
                    Ok(_) => "(no output)".to_string(),
                    Err(e) => e.clone(),
                }
            } else {
                c.raw.clone()
            };
        }
        match c.result {
            Ok(s) => {
                self.db.log_suggestion(
                    session_id,
                    &file,
                    start,
                    end,
                    &c.model,
                    Some(s.action.as_str()),
                    Some(&s.comment),
                    Some(&s.justification),
                    s.latency_ms,
                    None,
                );
                if self.settings.blind_review {
                    self.note("model", &format!("{} replied ({} ms)", c.model, s.latency_ms));
                } else {
                    self.note(
                        "model",
                        &format!("{} → {} ({} ms)", c.model, s.action.label(), s.latency_ms),
                    );
                }
                if let Some(slot) = self.candidates.get_mut(c.slot_idx) {
                    *slot = CandidateState::Ready(s);
                }
            }
            Err(e) => {
                self.db
                    .log_suggestion(session_id, &file, start, end, &c.model, None, None, None, 0, Some(&e));
                self.note("model", &format!("{} failed: {}", c.model, truncate(&e, 120)));
                if let Some(slot) = self.candidates.get_mut(c.slot_idx) {
                    *slot = CandidateState::Failed(e);
                }
            }
        }
    }

    pub fn open_settings(&mut self) {
        if self.screen != Screen::Settings {
            self.prev_screen = self.screen;
            self.screen = Screen::Settings;
        }
    }

    pub fn close_settings(&mut self) {
        self.settings.save(&self.db);
        self.note("settings", "saved");
        self.screen = self.prev_screen;
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
        if self
            .candidates
            .iter()
            .any(|c| matches!(c, CandidateState::Pending))
            || self.prs_loading
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }

        self.global_hotkeys(ctx);
        crate::ui::chrome::top_bar(self, ctx);
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
            Screen::Settings => self.ui_settings(ctx, ui),
        });
    }
}

impl CraApp {
    fn global_hotkeys(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers};
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Q)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
                Screen::Review => self.screen = Screen::FilePicker,
                Screen::FilePicker => self.screen = Screen::RefPicker,
                Screen::RefPicker | Screen::Summary => self.screen = Screen::RepoPicker,
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
    /// land after the first has already shifted the line numbers.
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
            app.plan = Some(Self::plan(&repo));
            Harness { app, repo, _dir: dir }
        }

        fn plan(repo: &TempRepo) -> ReviewPlan {
            let diff = gitio::review_diff(&repo.path(), "main", 12).expect("diff");
            let files = crate::diffparse::parse(&diff);
            let extracted = comments::extract_units(&files, 12);
            assert_eq!(extracted.len(), 1, "expected one reviewable file");
            let files = extracted
                .into_iter()
                .map(|(path, units)| ReviewFile { path, units, line_offset: 0, decided: 0 })
                .collect();
            ReviewPlan {
                session_id: 1,
                ref_kind: RefKind::Branch,
                ref_name: "feature".into(),
                base_ref: "main".into(),
                files,
                file_idx: 0,
                unit_idx: 0,
                decided_total: 0,
            }
        }

        /// Point every slot at a fake CLI and start the review, through the
        /// same entry point the file picker uses.
        fn enter_with(&mut self, slots: Vec<crate::settings::ModelSlot>) {
            self.app.settings.models = slots;
            self.app.start_review(&egui::Context::default(), 0);
        }

        /// Drain replies until every slot has settled, or give up.
        fn settle(&mut self) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                self.app.pump_messages();
                let pending = self
                    .app
                    .candidates
                    .iter()
                    .any(|c| matches!(c, CandidateState::Pending));
                if !pending {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "models never came back");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
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
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let mut h = Harness::new("reply");
        h.enter_with(vec![cli.slot("")]);
        h.settle();

        match &h.app.candidates[0] {
            CandidateState::Ready(s) => assert_eq!(s.action, Action::Rewrite),
            other => panic!("expected a ready candidate, got {}", state_name(other)),
        }
        // Picking it must reformat the prose as a comment in the unit's style.
        h.app.choose_candidate(0);
        assert_eq!(h.app.editor, "// Counts retries.");
        assert_eq!(h.app.chosen, Some(Choice::Candidate(0)));
    }

    fn state_name(s: &CandidateState) -> &'static str {
        match s {
            CandidateState::Disabled => "disabled",
            CandidateState::Pending => "pending",
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
        let second =
            FakeCli::new(&dir, "second", FakeCliSpec { reply: VERDICT, ..Default::default() });

        let mut h = Harness::new("session");
        let mut slot = first.slot("");
        slot.session_key = "conversation_id".into();
        slot.resume_command = format!("{} --conversation {{session}}", second.command());
        h.enter_with(vec![slot]);
        h.settle();

        assert_eq!(h.app.sessions[0].as_deref(), Some("sess-7"));
        assert!(h.app.can_ask(0), "a settled, resumable slot should accept a follow-up");

        h.app.follow_up = "too vague".into();
        h.app.ask_followup(&egui::Context::default(), None);
        h.settle();

        // The id must reach the resumed process, and only the new message with
        // it — the conversation itself lives in the CLI's session.
        let argv = second.argv_seen();
        assert!(argv.contains("--conversation sess-7"), "{argv}");
        let sent = second.stdin_seen();
        assert!(sent.contains("too vague"), "{sent}");
        assert!(!sent.contains("Increment the counter"), "the diff was re-sent: {sent}");
        assert!(h.app.follow_up.is_empty(), "the box should clear once sent");
        assert_eq!(h.app.convos[0].len(), 2, "both turns belong in the inspector");
    }

    #[test]
    fn a_slot_without_a_session_cannot_be_asked_again() {
        let dir = TempDir::new("nosession");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let mut h = Harness::new("nosession");
        // No session key and no {session} in the command: nothing to resume.
        h.enter_with(vec![cli.slot("")]);
        h.settle();

        assert!(!h.app.can_ask(0));
        h.app.follow_up = "why?".into();
        h.app.ask_followup(&egui::Context::default(), None);
        assert!(h.app.review_error.is_some(), "the user needs to be told why nothing happened");
        assert_eq!(h.app.follow_up, "why?", "an unsent message must not be cleared");
    }

    #[test]
    fn a_pending_slot_is_not_asked_again_mid_flight() {
        let dir = TempDir::new("pending");
        let cli = FakeCli::new(
            &dir,
            "fake",
            FakeCliSpec { reply: VERDICT, delay_secs: 30, ..Default::default() },
        );
        let mut h = Harness::new("pending");
        let mut slot = cli.slot("");
        slot.session_key = "conversation_id".into();
        slot.resume_command = format!("{} {{session}}", cli.command());
        h.enter_with(vec![slot]);

        // Still in flight: one reply per request keeps answers attributable.
        assert!(matches!(h.app.candidates[0], CandidateState::Pending));
        assert!(!h.app.can_ask(0));
    }

    #[test]
    fn saving_writes_the_indent_back_and_records_provenance() {
        let dir = TempDir::new("save");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let mut h = Harness::new("save");
        h.enter_with(vec![cli.slot("")]);
        h.settle();
        h.app.choose_candidate(0);

        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);

        let after = h.repo.read("src/lib.rs");
        assert!(after.contains("    // Counts retries."), "indent not restored: {after}");
        assert!(!after.contains("Increment the counter"), "{after}");
        // And it moved on to the next comment in the same file.
        assert_eq!(h.app.original_display, "// Reset the counter to zero");
    }

    #[test]
    fn a_second_edit_in_the_same_file_accounts_for_the_first() {
        let dir = TempDir::new("offset");
        let two_liner = "{\"action\":\"rewrite\",\"comment\":\"First line.\\nSecond line.\",\
\"justification\":\"needs two\"}";
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: two_liner, ..Default::default() });
        let mut h = Harness::new("offset");

        // First comment becomes two lines, pushing everything below it down.
        h.enter_with(vec![cli.slot("")]);
        h.settle();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert_eq!(h.app.plan.as_ref().unwrap().files[0].line_offset, 1);

        // The second edit has to land on the shifted lines, not the original.
        h.settle();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);

        let after = h.repo.read("src/lib.rs");
        assert_eq!(after.matches("// First line.").count(), 2, "both comments rewritten: {after}");
        assert!(after.contains("counter += 1;"), "{after}");
        assert!(after.contains("counter = 0;"), "{after}");
        assert!(!after.contains("Increment"), "{after}");
        assert!(!after.contains("Reset"), "{after}");
        assert_eq!(h.app.screen as u8, Screen::Summary as u8, "plan should be exhausted");
    }

    #[test]
    fn committing_records_the_model_as_co_author() {
        let dir = TempDir::new("commit");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let mut h = Harness::new("commit");
        h.enter_with(vec![cli.slot("")]);
        h.settle();
        h.app.choose_candidate(0);

        h.app.save_and_continue(&egui::Context::default(), true);
        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);

        let message = h.repo.git(&["log", "-1", "--format=%B"]);
        assert!(message.contains("review(comments): rewrite"), "{message}");
        assert!(message.contains("Comment-provenance: fake"), "{message}");
        assert!(message.contains("Co-authored-by: Fake <fake@example.com>"), "{message}");
        assert!(message.contains("says why"), "the model's reasoning belongs in the body: {message}");
    }

    #[test]
    fn keeping_the_original_touches_neither_the_file_nor_git() {
        let dir = TempDir::new("keep");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let mut h = Harness::new("keep");
        let before = h.repo.read("src/lib.rs");
        let head = gitio::head_sha(&h.repo.path()).unwrap();

        h.enter_with(vec![cli.slot("")]);
        h.settle();
        h.app.choose_keep();
        h.app.save_and_continue(&egui::Context::default(), true);

        assert!(h.app.review_error.is_none(), "{:?}", h.app.review_error);
        assert_eq!(h.repo.read("src/lib.rs"), before, "a keep must not rewrite the file");
        assert_eq!(gitio::head_sha(&h.repo.path()).unwrap(), head, "a keep must not commit");
    }

    #[test]
    fn a_failing_model_leaves_the_others_usable() {
        let dir = TempDir::new("mixed");
        let good = FakeCli::new(&dir, "good", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let bad = FakeCli::new(
            &dir,
            "bad",
            FakeCliSpec { reply: "boom", exit_code: 1, ..Default::default() },
        );
        let mut h = Harness::new("mixed");
        h.enter_with(vec![good.slot(""), bad.slot("")]);
        h.settle();

        assert!(matches!(h.app.candidates[0], CandidateState::Ready(_)));
        assert!(matches!(h.app.candidates[1], CandidateState::Failed(_)));
        h.app.choose_candidate(0);
        assert_eq!(h.app.chosen, Some(Choice::Candidate(0)));
    }

    #[test]
    fn blinding_hides_names_until_a_choice_is_made() {
        let dir = TempDir::new("blind");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let mut h = Harness::new("blind");
        h.app.settings.blind_review = true;
        let mut a = cli.slot("");
        a.name = "alpha".into();
        let mut b = cli.slot("");
        b.name = "beta".into();
        h.enter_with(vec![a, b]);
        h.settle();

        assert!(h.app.names_hidden());
        let order = h.app.candidate_order();
        // Labels follow screen position, not slot, so nothing identifies the
        // model behind a card.
        assert_eq!(h.app.slot_label(order[0], 0), "model A");
        assert_eq!(h.app.slot_label(order[1], 1), "model B");

        // Picking the first card must select whichever slot it stands for.
        h.app.choose_candidate(order[0]);
        assert_eq!(h.app.chosen, Some(Choice::Candidate(order[0])));
        // And once chosen, the names come back so provenance is visible.
        assert!(!h.app.names_hidden());
        assert!(h.app.slot_label(order[0], 0) != "model A");
    }

    #[test]
    fn blinding_off_leaves_the_order_and_the_names_alone() {
        let dir = TempDir::new("unblind");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let mut h = Harness::new("unblind");
        h.app.settings.blind_review = false;
        let mut a = cli.slot("");
        a.name = "alpha".into();
        h.enter_with(vec![a]);

        assert!(!h.app.names_hidden());
        assert_eq!(h.app.candidate_order(), vec![0]);
        assert_eq!(h.app.slot_label(0, 0), "alpha");
    }

    #[test]
    fn a_decision_records_whether_it_was_blinded() {
        let dir = TempDir::new("blindrec");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let mut h = Harness::new("blindrec");
        h.app.settings.blind_review = true;
        h.enter_with(vec![cli.slot("")]);
        h.settle();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);

        // An unblinded label is weaker evidence, so the report has to be able
        // to tell them apart after the fact.
        let rows = h.app.db.corpus(10);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].blinded);
        assert!(!rows[0].unit_json.is_empty(), "the unit must be stored for replay");
    }

    #[test]
    fn a_recheck_records_the_judgement_without_touching_the_file() {
        let dir = TempDir::new("recheck");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let mut h = Harness::new("recheck");

        // Decide one comment normally, which writes the file and stores a label.
        h.enter_with(vec![cli.slot("")]);
        h.settle();
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
        assert_eq!(h.repo.read("src/lib.rs"), after_first, "a re-check must not edit the tree");
        assert_eq!(gitio::head_sha(&h.repo.path()).unwrap(), head, "or commit");
        assert_eq!(h.app.db.corpus(10).len(), 2, "but it must record the second judgement");
    }

    #[test]
    fn a_recheck_with_no_history_explains_itself() {
        let mut h = Harness::new("nohistory");
        h.app.start_recheck(&egui::Context::default(), 10);
        assert!(h.app.ref_error.as_deref().is_some_and(|e| e.contains("no past decisions")));
    }

    #[test]
    fn repeated_judgements_give_the_report_a_noise_floor() {
        let dir = TempDir::new("floor");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let mut h = Harness::new("floor");
        h.enter_with(vec![cli.slot("")]);
        h.settle();
        h.app.choose_candidate(0);
        h.app.save_and_continue(&egui::Context::default(), false);

        // Before any repeat there is no scale to read agreement against.
        assert!(crate::eval::Report::from_db(&h.app.db).self_agreement_pct().is_none());

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
        assert!(report.render().contains("self-agreement: 0%"), "{}", report.render());
    }

    #[test]
    fn a_stale_reply_from_a_previous_comment_is_discarded() {
        let dir = TempDir::new("stale");
        let cli = FakeCli::new(&dir, "fake", FakeCliSpec { reply: VERDICT, ..Default::default() });
        let mut h = Harness::new("stale");
        h.enter_with(vec![cli.slot("")]);

        // The user moves on before the model answers; the late reply carries
        // the old sequence number and must not overwrite the new question.
        let stale_seq = h.app.review_seq;
        h.app.skip_unit(&egui::Context::default());
        assert!(h.app.review_seq > stale_seq);

        h.app.handle_candidate(CandidateMsg {
            seq: stale_seq,
            slot_idx: 0,
            model: "fake".into(),
            result: Ok(Suggestion {
                action: Action::Delete,
                comment: String::new(),
                justification: "from the previous comment".into(),
                latency_ms: 1,
            }),
            raw: String::new(),
        });
        assert!(
            !matches!(h.app.candidates[0], CandidateState::Ready(_)),
            "a stale reply was shown against the wrong comment"
        );
    }
}
