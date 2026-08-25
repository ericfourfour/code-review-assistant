use egui::{Key, Modifiers, RichText};

use crate::app::{CraApp, Screen, WholeBranchReviewState};
use crate::ui::theme;

impl CraApp {
    pub fn ui_summary(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let typing = ctx.wants_keyboard_input();
        if !typing {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F)) {
                self.screen = Screen::FilePicker;
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::B)) {
                self.load_refs();
                self.screen = Screen::RefPicker;
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
                    ui.label(RichText::new(format!("{decided} / {}", p.total_units())).monospace());
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
            if ui.button("Back to files [F]").clicked() {
                self.screen = Screen::FilePicker;
            }
            if ui.button("Another branch/PR [B]").clicked() {
                self.load_refs();
                self.screen = Screen::RefPicker;
            }
            if ui.button("Another repo [Esc]").clicked() {
                self.screen = Screen::RepoPicker;
            }
        });

        let recheck = self.plan.as_ref().is_some_and(|p| p.is_recheck());
        if self.plan.is_some() && !recheck {
            ui.add_space(10.0);
            ui.separator();
            self.whole_branch_review_section(ctx, ui);
        }
        self.evidence_window(ctx);
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
                                    if let Some(a) = snap.activity_line() {
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
                                WholeBranchReviewState::Idle => {}
                            }
                            ui.add_space(10.0);
                        }
                    }
                }
            });
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
