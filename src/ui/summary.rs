use egui::{Key, Modifiers, RichText};

use crate::app::{CraApp, PublishState, Screen, WholeBranchReviewState};
use crate::ui::theme;

impl CraApp {
    pub fn ui_summary(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let typing = ctx.wants_keyboard_input();
        if !typing {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F)) {
                self.goto(Screen::FilePicker);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::B)) {
                self.load_refs();
                self.goto(Screen::RefPicker);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::G))
                && !self.whole_branch_review_running()
            {
                self.start_whole_branch_review(ctx);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::N)) {
                self.open_followup();
                return;
            }
            // Both routes move the same commits, so the keys are as guarded as
            // the buttons: nothing to publish, or one already in flight, and
            // the key does nothing rather than starting a second attempt.
            let deliverable =
                self.delivery.as_ref().is_some_and(|d| d.can_push()) && !self.publish.running();
            if deliverable {
                if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::P)) {
                    self.start_push();
                }
                let stackable = self.plan.as_ref().is_some_and(|p| !p.reviews_uncommitted());
                if stackable && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::S)) {
                    self.start_stacked_pr();
                }
            }
        }

        // An early exit opens this screen too (End session on the review screen), and
        // a summary that says "complete" over a partially reviewed plan would be
        // claiming a review that never happened.
        let complete = self
            .plan
            .as_ref()
            .map(|p| p.decided_total >= p.total_units())
            .unwrap_or(true);
        ui.heading(if complete {
            "Review complete"
        } else {
            "Review in progress"
        });
        if !complete {
            ui.label(theme::dim(
                "the session was ended early — undecided units resume from the file picker [F]",
            ));
        }
        // Reopened rather than reviewed: every unit was decided in an earlier
        // session, so this screen is here for the commits, not the verdicts.
        if self.plan.as_ref().is_some_and(|p| p.nothing_left()) {
            ui.label(theme::dim(
                "nothing left to decide here — this is the earlier session, reopened so its \
                 commits can be delivered. Untick \"skip decided\" in settings to judge the \
                 units again.",
            ));
        }
        ui.add_space(6.0);
        if let Some(p) = &self.plan {
            let (decided, committed) = self.db.decision_counts(p.session_id);
            egui::Grid::new("summary_grid")
                .num_columns(2)
                .spacing([12.0, 3.0])
                .show(ui, |ui| {
                    ui.label(theme::dim("ref"));
                    ui.label(
                        RichText::new(format!("{} [{}]", p.ref_name, p.ref_kind.label()))
                            .monospace(),
                    );
                    ui.end_row();
                    ui.label(theme::dim("base"));
                    ui.label(RichText::new(&p.base_ref).monospace());
                    ui.end_row();
                    ui.label(theme::dim("units reviewed"));
                    ui.label(
                        RichText::new(format!("{decided} / {}", p.reported_units())).monospace(),
                    );
                    ui.end_row();
                    ui.label(theme::dim("commits made"));
                    ui.label(RichText::new(committed.to_string()).monospace());
                    ui.end_row();
                    ui.label(theme::dim("session"));
                    ui.label(RichText::new(format!("#{}", p.session_id)).monospace());
                    ui.end_row();
                });
            ui.add_space(6.0);
            ui.label(theme::dim(
                "saved-but-uncommitted edits are in the working tree — commit them with git when ready",
            ));
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let open_notes = self
                .repo
                .as_ref()
                .map(|r| self.db.count_open_notes(&r.path))
                .unwrap_or(0);
            if ui
                .button(format!("Follow-up notes ({open_notes}) [N]"))
                .on_hover_text(
                    "triage the notes left during review, then hand the checked ones to a \
                     model with room for bigger changes",
                )
                .clicked()
            {
                self.open_followup();
                return;
            }
            ui.separator();
            if ui
                .button("Model evaluation [Ctrl+E]")
                .on_hover_text(
                    "which model's suggestions you actually take, and what each one costs",
                )
                .clicked()
            {
                self.open_eval();
                return;
            }
            if let Some(at) = self.review_in_progress() {
                if ui
                    .button(format!("◀ Back to {at}"))
                    .on_hover_text("returns to the unit you left — nothing is re-asked")
                    .clicked()
                {
                    self.goto(Screen::Review);
                }
            }
            if ui.button("Back to files [F]").clicked() {
                self.goto(Screen::FilePicker);
            }
            if ui.button("Another branch/PR [B]").clicked() {
                self.load_refs();
                self.goto(Screen::RefPicker);
            }
            if ui.button("Another repo [Esc]").clicked() {
                self.goto(Screen::RepoPicker);
            }
        });

        let recheck = self.plan.as_ref().is_some_and(|p| p.is_recheck());
        if self.plan.is_some() && !recheck {
            // Above the whole-branch review, because that section ends in a
            // scroll area that takes the rest of the window — anything after
            // it would be off the bottom of the screen.
            ui.add_space(10.0);
            ui.separator();
            self.publish_section(ui);
            ui.add_space(10.0);
            ui.separator();
            self.whole_branch_review_section(ctx, ui);
        }
        self.evidence_window(ctx);
    }

    /// Where the review's commits go once the reviewing is over.
    ///
    /// The fixes were committed in a worktree nobody else can see, on a branch
    /// that may not even be checked out anywhere the reviewer would look. Both
    /// routes out are offered together because choosing between them is a
    /// question about the branch rather than about git: push when the fixes
    /// belong on the branch that was reviewed, stack when they should be
    /// proposed back to whoever owns it.
    fn publish_section(&mut self, ui: &mut egui::Ui) {
        // Read out of the state rather than holding a borrow of it: the
        // buttons below need `self` back, and `unpushed` is as long as the
        // branch is — nothing here should be cloning it every repaint.
        let Some(d) = self.delivery.as_ref() else {
            return;
        };
        let (branch, remote, upstream) = (d.branch.clone(), d.remote.clone(), d.upstream.clone());
        let (ahead, behind, dirty) = (d.ahead(), d.behind, d.dirty);
        let deliverable = d.can_push();
        // Where the commits being published actually are, when that is not
        // this checkout — a detached review whose commits have since been
        // rescued onto a branch of their own.
        let elsewhere = (d.tip != "HEAD").then(|| {
            d.tip_branch
                .clone()
                .unwrap_or_else(|| format!("commit {}", &d.tip[..8.min(d.tip.len())]))
        });
        let Some(plan) = self.plan.as_ref() else {
            return;
        };
        let ref_name = plan.ref_name.clone();
        let what = plan.ref_kind.label();
        // A stacked pull request needs a committed branch to sit on top of. A
        // working-tree review has none: its commits *are* the branch tip.
        let stackable = !plan.reviews_uncommitted();
        let base = self.stack.base.clone();
        let restore_blocker = self.restore_blocker();
        let running = self.publish.running();
        let target = match (&upstream, &remote) {
            (Some(u), _) => u.clone(),
            (None, Some(r)) => format!("{r}/{ref_name} (new)"),
            (None, None) => "nowhere — this repository has no remote".to_string(),
        };

        theme::section_title(ui, "DELIVER — PUSH, OR STACK A PULL REQUEST");

        // A finished publish moved the very commits the controls describe, and
        // after a stacked one the worktree is on a different branch entirely.
        // Re-offering the buttons over that would invite a second publish that
        // undoes the first, so the outcome stands alone until it is dismissed.
        if let PublishState::Done(outcome) = &self.publish {
            ui.colored_label(theme::GOOD, format!("✔ {}", outcome.headline));
            if let Some(url) = &outcome.url {
                ui.hyperlink_to(RichText::new(url).monospace(), url);
            }
            for line in &outcome.detail {
                ui.label(theme::dim(line));
            }
            ui.add_space(4.0);
            if ui
                .button("↻ Publish again")
                .on_hover_text("re-reads where the commits stand and offers both routes again")
                .clicked()
            {
                self.publish = PublishState::Idle;
                self.refresh_delivery();
            }
            return;
        }

        // What is actually here, before either button claims it can move it.
        ui.horizontal_wrapped(|ui| {
            let from = match &elsewhere {
                Some(where_) => where_.clone(),
                None => branch.clone().unwrap_or_else(|| "HEAD (detached)".into()),
            };
            ui.label(RichText::new(from).monospace());
            ui.label(theme::dim("→"));
            ui.label(RichText::new(target).monospace().color(theme::ACCENT));
            ui.separator();
            ui.label(
                RichText::new(format!("{ahead} commit(s) to publish")).color(if ahead == 0 {
                    theme::TEXT_DIM
                } else {
                    theme::GOOD
                }),
            );
            if behind > 0 {
                ui.separator();
                ui.colored_label(theme::WARN, format!("{behind} behind"));
            }
            if dirty {
                ui.separator();
                ui.colored_label(
                    theme::WARN,
                    "uncommitted edits — not published by either route",
                );
            }
        });
        if let Some(where_) = &elsewhere {
            // The case that used to read "1 commit made · 0 to publish": the
            // session's commits are not on this checkout at all.
            ui.label(theme::dim(&format!(
                "this session's commits are not on this checkout — they are on {where_}, saved \
                 there when the worktree was re-pointed. They are what gets published."
            )));
        } else if branch.is_none() {
            ui.label(theme::dim(&format!(
                "the review ran detached because {ref_name} was checked out elsewhere — the \
                 commits are real and can still be published under that name"
            )));
        }
        ui.add_space(6.0);

        // -- push ---------------------------------------------------------
        let can_push = deliverable && behind == 0 && !running;
        ui.horizontal_wrapped(|ui| {
            let where_to = match &remote {
                Some(r) => format!("{r}/{ref_name}"),
                None => ref_name.clone(),
            };
            if ui
                .add_enabled(
                    can_push,
                    egui::Button::new(
                        RichText::new(format!("⬆ Push {ahead} commit(s) to {where_to} [P]"))
                            .strong(),
                    ),
                )
                .on_hover_text(format!(
                    "the fixes land on the {what} itself — for your own branch, or one you have \
                     write access to and agreed to fix directly"
                ))
                .clicked()
            {
                self.start_push();
            }
            if !can_push {
                ui.label(theme::dim(if running {
                    "waiting for the one in flight to finish"
                } else if remote.is_none() {
                    "no remote configured"
                } else if ahead == 0 {
                    "the remote already has every commit here"
                } else {
                    "the remote branch has moved — pull it in, or stack instead"
                }));
            }
        });

        // -- stack ---------------------------------------------------------
        if stackable {
            ui.add_space(4.0);
            egui::CollapsingHeader::new(RichText::new("⎇ Stacked pull request").strong())
                .id_salt("stacked_pr")
                // Open by default exactly when it is the better route: a push
                // that would be refused leaves this as the only way out.
                .default_open(behind > 0)
                .show(ui, |ui| {
                    ui.label(theme::dim(&format!(
                        "puts the fixes on a branch of their own and opens a pull request into \
                         {base}, so whoever owns the {what} reviews the reviewer. Needs `gh`."
                    )));
                    ui.add_space(4.0);
                    egui::Grid::new("stack_form")
                        .num_columns(2)
                        .spacing([10.0, 4.0])
                        .show(ui, |ui| {
                            ui.label(theme::dim("branch"));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.stack.branch)
                                    .desired_width(320.0)
                                    .font(egui::TextStyle::Monospace),
                            );
                            ui.end_row();
                            ui.label(theme::dim("into"));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.stack.base)
                                    .desired_width(320.0)
                                    .font(egui::TextStyle::Monospace),
                            )
                            .on_hover_text(
                                "the branch the pull request targets — the reviewed branch by \
                                 default, so the stack is a real stack",
                            );
                            ui.end_row();
                            ui.label(theme::dim("title"));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.stack.title)
                                    .desired_width(420.0),
                            );
                            ui.end_row();
                            ui.label(theme::dim("body"));
                            ui.add(
                                egui::TextEdit::multiline(&mut self.stack.body)
                                    .desired_width(420.0)
                                    .desired_rows(3),
                            );
                            ui.end_row();
                        });

                    // Without this the fixes stay on the reviewed branch as
                    // well, and the next push of that branch quietly swallows
                    // the pull request just opened for them.
                    match &restore_blocker {
                        None => {
                            ui.checkbox(
                                &mut self.stack.restore,
                                format!(
                                    "put {ref_name} back at {} afterwards",
                                    upstream.as_deref().unwrap_or_default()
                                ),
                            )
                            .on_hover_text(
                                "the fixes then live on the new branch only, so the reviewed \
                                 branch matches the remote again and cannot swallow the pull \
                                 request",
                            );
                        }
                        Some(why) => {
                            ui.label(theme::dim(&format!("{ref_name} is left as it is: {why}")));
                        }
                    }
                    ui.add_space(4.0);
                    if ui
                        .add_enabled(
                            deliverable && !running,
                            egui::Button::new(
                                RichText::new(format!("⎇ Open pull request into {base} [S]"))
                                    .strong(),
                            ),
                        )
                        .clicked()
                    {
                        self.start_stacked_pr();
                    }
                });
        }

        // -- what happened --------------------------------------------------
        match &self.publish {
            PublishState::Running(stage) => {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(theme::dim(stage));
                });
            }
            PublishState::Failed(e) => {
                ui.add_space(6.0);
                ui.colored_label(theme::BAD, format!("✕ {e}"));
            }
            // Handled above, before anything that could start a second one.
            PublishState::Idle | PublishState::Done(_) => {}
        }
    }

    /// The whole-branch review finds interactions that isolated unit reviews
    /// cannot show.
    fn whole_branch_review_section(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        theme::section_title(ui, "WHOLE-BRANCH REVIEW — CROSS-CUTTING FINDINGS");
        ui.label(theme::dim(
            "Each unit was judged on its own. This review gives every enabled model the whole \
branch diff (and the repository) to look for what no single unit shows: hunks that contradict \
each other, half-applied renames, dead code left behind, the missing test. Findings are \
recorded and yours to dismiss — nothing is edited.",
        ));
        ui.add_space(4.0);

        let running = self.whole_branch_review_running();
        ui.horizontal(|ui| {
            let label = if self.whole_branch_review.is_empty() {
                "▶ Run whole-branch review [G]"
            } else {
                "↻ Re-run whole-branch review [G]"
            };
            if ui
                .add_enabled(!running, egui::Button::new(RichText::new(label).strong()))
                .clicked()
            {
                self.start_whole_branch_review(ctx);
            }
            let open = self.findings.iter().filter(|r| !r.dismissed).count();
            if open > 0
                && ui
                    .button("⧉ copy findings")
                    .on_hover_text("as markdown")
                    .clicked()
            {
                ctx.copy_text(self.findings_markdown());
            }
        });

        // Per-model status row.
        if !self.whole_branch_review.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for (i, model_config) in self.settings.models.iter().enumerate() {
                    match self.whole_branch_review.get(i) {
                        Some(WholeBranchReviewState::Idle) | None => continue,
                        Some(state) => {
                            ui.label(
                                RichText::new(&model_config.name)
                                    .monospace()
                                    .small()
                                    .color(theme::model_color(i)),
                            );
                            match state {
                                WholeBranchReviewState::Pending(live) => {
                                    ui.spinner();
                                    let snap = live.snapshot();
                                    // The whole-branch review runs on a doubled deadline.
                                    ui.label(theme::dim(&snap.clock(
                                        self.settings.model_timeout_secs.saturating_mul(2),
                                    )));
                                    if let Some(a) = snap.activity_line(false) {
                                        ui.label(theme::dim(&a));
                                    }
                                }
                                WholeBranchReviewState::Done { n, latency_ms } => {
                                    ui.label(theme::dim(&format!(
                                        "{n} finding(s) · {latency_ms} ms"
                                    )));
                                }
                                WholeBranchReviewState::Failed(e) => {
                                    ui.colored_label(theme::BAD, crate::app::truncate(e, 90));
                                }
                                WholeBranchReviewState::Paused(p) => {
                                    theme::badge(ui, "PAUSED", theme::WARN);
                                    ui.label(theme::dim(&p.line(true)));
                                }
                                WholeBranchReviewState::Idle => {}
                            }
                            ui.add_space(10.0);
                        }
                    }
                }
            });
        }

        // A model whose branch pass was stopped when the reviewer left this
        // page: (model index, continue the same session?).
        let mut rerun: Option<(usize, bool)> = None;
        for (i, state) in self.whole_branch_review.iter().enumerate() {
            let WholeBranchReviewState::Paused(call) = state else {
                continue;
            };
            // The branch pass is never blinded — its findings are attributed by
            // name on this very screen — so the card shows everything.
            let view = crate::ui::procs_panel::PausedView {
                name: &call.model,
                identifying: true,
                restart_label: "↻ Run again",
            };
            match crate::ui::procs_panel::paused_row(ui, call, &self.settings, view) {
                crate::ui::procs_panel::PausedAction::Resume => rerun = Some((i, true)),
                crate::ui::procs_panel::PausedAction::Restart => rerun = Some((i, false)),
                crate::ui::procs_panel::PausedAction::None => {}
            }
        }
        if let Some((i, resume)) = rerun {
            self.rerun_branch_model(ctx, i, resume);
        }

        let mut dismiss: Option<i64> = None;
        let mut evidence_click: Option<crate::models::Evidence> = None;
        egui::ScrollArea::vertical()
            .id_salt("findings_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for row in self.findings.iter().filter(|r| !r.dismissed) {
                    let f = &row.finding;
                    let sev = f.severity.trim().to_ascii_lowercase();
                    let sev_color = match sev.as_str() {
                        "high" => theme::BAD,
                        "medium" => theme::WARN,
                        _ => theme::TEXT_DIM,
                    };
                    egui::Frame::group(ui.style())
                        .fill(theme::PANEL)
                        .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(50)))
                        .inner_margin(egui::Margin::same(6.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                theme::badge(
                                    ui,
                                    if sev.is_empty() { "?" } else { &sev },
                                    sev_color,
                                );
                                ui.label(RichText::new(&f.title).strong());
                                ui.label(theme::dim(&format!("· {}", row.model)));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .small_button("✕ dismiss")
                                            .on_hover_text(
                                                "keep it on the record, marked dismissed",
                                            )
                                            .clicked()
                                        {
                                            dismiss = Some(row.id);
                                        }
                                    },
                                );
                            });
                            if !f.files.is_empty() {
                                ui.label(
                                    RichText::new(f.files.join("  "))
                                        .monospace()
                                        .small()
                                        .color(theme::ACCENT),
                                );
                            }
                            ui.label(
                                RichText::new(&f.detail)
                                    .color(egui::Color32::from_rgb(196, 208, 220)),
                            );
                            if !f.evidence.is_empty() {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(theme::dim("read:"));
                                    for ev in &f.evidence {
                                        let label = if ev.lines.trim().is_empty() {
                                            ev.file.clone()
                                        } else {
                                            format!("{}:{}", ev.file, ev.lines)
                                        };
                                        let resp = ui
                                            .small_button(RichText::new(label).monospace().small());
                                        let resp = if ev.note.trim().is_empty() {
                                            resp
                                        } else {
                                            resp.on_hover_text(&ev.note)
                                        };
                                        if resp.clicked() {
                                            evidence_click = Some(ev.clone());
                                        }
                                    }
                                });
                            }
                        });
                    ui.add_space(4.0);
                }
                let done =
                    !self.whole_branch_review.is_empty() && !self.whole_branch_review_running();
                if done && self.findings.iter().all(|r| r.dismissed) {
                    ui.label(theme::dim(if self.findings.is_empty() {
                        "no cross-cutting findings reported"
                    } else {
                        "all findings dismissed"
                    }));
                }
            });
        if let Some(id) = dismiss {
            self.dismiss_finding(id);
        }
        if let Some(ev) = evidence_click {
            self.show_evidence = Some(ev);
        }
    }
}
