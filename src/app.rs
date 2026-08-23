//! Top-level egui application: state, screen routing, hotkeys, async plumbing.

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};

use crate::comments::{self, CommentUnit};
use crate::db::Db;
use crate::gitio::{self, BranchInfo, PrInfo};
use crate::models::{self, Action, CandidateMsg, Suggestion, Turn};
use crate::review::{self, Choice, RefKind, ReviewFile, ReviewPlan};
use crate::settings::{ModelSlot, Settings};

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
    /// Snapshot of the models that own `candidates`. Settings can be edited
    /// mid-review, but a result's name and co-author must never change slots.
    pub candidate_models: Vec<ModelSlot>,
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
            candidate_models: Vec::new(),
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
        self.candidate_models = self.settings.models.clone();
        self.candidates = self
            .candidate_models
            .iter()
            .map(|m| if m.enabled { CandidateState::Pending } else { CandidateState::Disabled })
            .collect();
        self.convos = vec![Vec::new(); self.candidate_models.len()];
        self.sessions = vec![None; self.candidate_models.len()];
        self.follow_up.clear();
        self.show_prompt = None;
        let enabled_models: Vec<_> = self
            .candidate_models
            .iter()
            .cloned()
            .enumerate()
            .filter(|(_, model)| model.enabled)
            .collect();
        for (idx, slot) in enabled_models {
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
            .candidate_models
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
            let Some(slot_cfg) = self.candidate_models.get(idx).cloned() else { continue };
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
            self.candidate_models
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
        let name = self
            .candidate_models
            .get(slot_idx)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| format!("model {slot_idx}"));
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
            Some(Choice::Candidate(i)) => self
                .candidate_models
                .get(*i)
                .map(|m| (m.name.clone(), m.coauthor.clone())),
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

        // Apply to the working tree when the text changed.
        let new_lines = unit.reindent(&self.editor);
        let final_text = new_lines.join("\n");
        let mut delta = 0i64;
        if action != Action::Keep {
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
        let mut commit_error = None;
        if commit {
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
                        // `git add` may already have succeeded, and the edit is
                        // definitely on disk. Record and advance it as an
                        // uncommitted decision so retrying never reapplies the
                        // unit at stale line numbers.
                        commit_error = Some(e);
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
        if let Some(e) = commit_error {
            self.review_error = Some(e.clone());
            self.note("error", &format!("edit saved but commit failed: {e}"));
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
        if let Some(key) = self
            .candidate_models
            .get(c.slot_idx)
            .map(|m| m.session_key.clone())
        {
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
