//! The follow-up screen: triage the notes parked during review, then hand the
//! checked ones to a single model in an interactive fix session.
//!
//! The per-unit review cannot act beyond a unit's own lines, so anything bigger
//! collected here as a note. Triage is three-way: dismiss (never see it again),
//! check (the next session's job — marked resolved when it launches), or
//! leave unchecked (still here next visit). The prompt preamble is editable
//! and the model is picked per session, because "done with the small model"
//! is exactly when a larger one earns its keep.

use egui::RichText;

use crate::app::CraApp;
use crate::ui::{code, theme};

impl CraApp {
    pub fn ui_followup(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.heading("Follow-up");
        ui.label(theme::dim(
            "Notes left during review, each pinned to the unit that revealed it. Check the ones \
the fix session should tackle and dismiss the rest; unchecked notes wait for next time. \
Checked notes are marked resolved the moment the session starts — the transcript on the \
right is where their fate is read.",
        ));
        if let Some(err) = self.fix_error.clone() {
            ui.colored_label(theme::BAD, err);
        }
        ui.add_space(4.0);

        let mut dismiss: Option<i64> = None;
        let mut reset_prompt = false;
        let mut start = false;
        let mut send = false;
        let mut resume_paused = false;
        let mut restart_paused = false;
        let can_resume = self.fix_can_resume();
        let model_options: Vec<(usize, String)> = self
            .settings
            .enabled_models()
            .into_iter()
            .map(|(i, m)| (i, m.name))
            .collect();

        ui.columns(2, |cols| {
            // ---- left: the backlog ----
            {
                let ui = &mut cols[0];
                let n_checked = self.notes.iter().filter(|r| r.checked).count();
                ui.horizontal(|ui| {
                    theme::section_title(ui, "OPEN NOTES");
                    ui.label(theme::dim(&format!(
                        "{} open · {n_checked} checked",
                        self.notes.len()
                    )));
                });
                egui::ScrollArea::vertical()
                    .id_salt("notes_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for row in self.notes.iter_mut() {
                            egui::Frame::group(ui.style())
                                .fill(if row.checked { theme::RAISED } else { theme::PANEL })
                                .stroke(egui::Stroke::new(
                                    1.0_f32,
                                    if row.checked {
                                        theme::ACCENT
                                    } else {
                                        egui::Color32::from_gray(50)
                                    },
                                ))
                                .inner_margin(egui::Margin::same(6.0))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.checkbox(&mut row.checked, "")
                                            .on_hover_text("hand this note to the fix session");
                                        ui.label(
                                            RichText::new(row.note.locus())
                                                .monospace()
                                                .strong()
                                                .color(theme::ACCENT),
                                        );
                                        // The date is enough; the millis are for the record.
                                        ui.label(theme::dim(
                                            &row.note.ts[..16.min(row.note.ts.len())],
                                        ));
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui
                                                    .small_button("✕ dismiss")
                                                    .on_hover_text(
                                                        "keep it on the record, marked dismissed \
                                                         — it is never shown again",
                                                    )
                                                    .clicked()
                                                {
                                                    dismiss = Some(row.note.id);
                                                }
                                            },
                                        );
                                    });
                                    ui.label(
                                        RichText::new(&row.note.text)
                                            .color(egui::Color32::from_rgb(196, 208, 220)),
                                    );
                                    if !row.note.excerpt.trim().is_empty() {
                                        let file = row.note.file.clone();
                                        let excerpt = row.note.excerpt.clone();
                                        egui::CollapsingHeader::new(theme::dim(
                                            "the code at review time",
                                        ))
                                        .id_salt(("note_excerpt", row.note.id))
                                        .show(ui, |ui| {
                                            egui::Frame::none()
                                                .fill(theme::CODE_BG)
                                                .inner_margin(egui::Margin::same(4.0))
                                                .show(ui, |ui| {
                                                    code::show_block(
                                                        ui,
                                                        &file,
                                                        &excerpt,
                                                        theme::CODE,
                                                    );
                                                });
                                        });
                                    }
                                });
                            ui.add_space(4.0);
                        }
                        if self.notes.is_empty() {
                            ui.label(theme::dim(
                                "no open notes — leave one from the review screen's NOTE box \
                                 when a unit reveals something bigger than itself",
                            ));
                        }
                    });
            }
            // ---- right: prompt, model, session ----
            {
                let ui = &mut cols[1];
                ui.horizontal(|ui| {
                    theme::section_title(ui, "PROMPT");
                    if ui.small_button("reset to default").clicked() {
                        reset_prompt = true;
                    }
                    ui.label(theme::dim(
                        "— the checked notes are appended, each with its locus and code",
                    ));
                });
                ui.add(
                    egui::TextEdit::multiline(&mut self.fix_prompt)
                        .id(egui::Id::new("fix_prompt_box"))
                        .desired_rows(5)
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    theme::section_title(ui, "MODEL");
                    let current = self
                        .settings
                        .models
                            .get(self.selected_fix_model_index)
                        .map(|m| m.name.clone())
                        .unwrap_or_else(|| "—".into());
                    egui::ComboBox::from_id_salt("fix_model_pick")
                        .selected_text(RichText::new(current).monospace())
                        .show_ui(ui, |ui| {
                            for (idx, name) in &model_options {
                                ui.selectable_value(
                            &mut self.selected_fix_model_index,
                                    *idx,
                                    RichText::new(name).monospace(),
                                );
                            }
                        });
                    let n_checked = self.notes.iter().filter(|r| r.checked).count();
                    let ready = n_checked > 0 && !self.fix_running && !model_options.is_empty();
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(RichText::new("▶ Begin fix session").strong()),
                        )
                        .on_hover_text(
                            "opens a fresh interactive session on the checked notes and marks \
                             them resolved",
                        )
                        .clicked()
                    {
                        start = true;
                    }
                    if self.fix_running {
                        ui.spinner();
                        // The fix session runs on a quadrupled deadline.
                        let deadline = self.settings.model_timeout_secs.saturating_mul(4);
                        match self.fix_proc.as_ref().map(|l| l.snapshot()) {
                            Some(snap) => {
                                ui.label(theme::dim(&format!(
                                    "working… {} · {}",
                                    snap.clock(deadline),
                                    snap.pid_label()
                                )));
                                if let Some(a) = snap.activity_line() {
                                    ui.label(theme::dim(&a));
                                }
                            }
                            None => {
                                ui.label(theme::dim("working…"));
                            }
                        }
                    }
                });
                if model_options.is_empty() {
                    ui.label(theme::dim("no models enabled — enable one in settings (Ctrl+,)"));
                }
                ui.add_space(4.0);

                theme::section_title(ui, "SESSION");
                // The turn that was cut short when the reviewer left this
                // page. Nothing has been restarted for them.
                if let Some(call) = &self.fix_paused {
                    match crate::ui::procs_panel::paused_row(
                        ui,
                        call,
                        &self.settings,
                        "↻ Start a new session",
                    ) {
                        crate::ui::procs_panel::PausedAction::Resume => resume_paused = true,
                        crate::ui::procs_panel::PausedAction::Restart => restart_paused = true,
                        crate::ui::procs_panel::PausedAction::None => {}
                    }
                }
                if self.fix_convo.is_empty() {
                    ui.label(theme::dim("no session yet — check a note and begin"));
                } else {
                    // The transcript takes what the follow-up row below does not.
                    let transcript_h = (ui.available_height() - 34.0).max(90.0);
                    egui::ScrollArea::vertical()
                        .id_salt("fix_transcript")
                        .max_height(transcript_h)
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for (i, t) in self.fix_convo.iter().enumerate() {
                                theme::section_title(ui, &format!("SENT — turn {}", i + 1));
                                ui.label(RichText::new(&t.prompt).monospace().color(theme::TEXT_DIM));
                                ui.add_space(4.0);
                                theme::section_title(ui, &format!("RECEIVED — turn {}", i + 1));
                                if t.reply.is_empty() {
                                    ui.horizontal(|ui| {
                                        ui.spinner();
                                        match self.fix_proc.as_ref().map(|l| l.snapshot()) {
                                            Some(snap) => {
                                                let deadline = self
                                                    .settings
                                                    .model_timeout_secs
                                                    .saturating_mul(4);
                                                ui.label(theme::dim(&format!(
                                                    "working… {}",
                                                    snap.clock(deadline)
                                                )));
                                                if let Some(a) = snap.activity_line() {
                                                    ui.label(theme::dim(&a));
                                                }
                                            }
                                            None => {
                                                ui.label(theme::dim("working…"));
                                            }
                                        }
                                    });
                                } else {
                                    ui.label(RichText::new(&t.reply).monospace());
                                }
                                ui.add_space(8.0);
                            }
                        });
                    ui.horizontal(|ui| {
                        let clicked = ui
                            .add_enabled(
                                can_resume && !self.fix_follow_up.trim().is_empty(),
                                egui::Button::new("Send"),
                            )
                            .clicked();
                        let resp = ui.add_enabled(
                            can_resume,
                            egui::TextEdit::singleline(&mut self.fix_follow_up)
                                .id(egui::Id::new("fix_follow_up_box"))
                                .desired_width(f32::INFINITY)
                                .hint_text(
                                    "continue the session, e.g. “also update the tests you broke”",
                                ),
                        );
                        let submitted = resp.lost_focus()
                            && ctx.input(|i| {
                                i.key_pressed(egui::Key::Enter) && !i.modifiers.shift
                            });
                        send = clicked || submitted;
                    });
                }
            }
        });

        if reset_prompt {
            self.fix_prompt = crate::notes::default_preamble().to_string();
        }
        if let Some(id) = dismiss {
            self.dismiss_note(id);
        }
        if resume_paused {
            self.resume_fix(ctx);
        }
        if restart_paused {
            // A new session starts from the notes again, so the stopped one is
            // let go of rather than left offering a resume that no longer
            // matches what is on screen.
            self.fix_paused = None;
            self.fix_convo.clear();
            self.fix_session = None;
            self.note("follow-up", "dropped the paused session — check notes and begin again");
        }
        if start {
            self.start_fix_session(ctx);
        }
        if send {
            self.ask_fix_followup(ctx);
        }
    }
}
