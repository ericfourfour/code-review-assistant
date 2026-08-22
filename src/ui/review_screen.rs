//! The one-comment-at-a-time review screen: context, three model candidates,
//! editable final text, save/commit continuations.

use egui::{Key, Modifiers, RichText};

use crate::app::{CandidateState, CraApp};
use crate::models::Action;
use crate::review::{self, Choice};
use crate::ui::theme;

const NUM_KEYS: [Key; 9] = [
    Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5,
    Key::Num6, Key::Num7, Key::Num8, Key::Num9,
];

impl CraApp {
    pub fn ui_review(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let Some(unit) = self.current_unit() else {
            ui.label(theme::dim("nothing to review"));
            return;
        };
        let typing = ctx.wants_keyboard_input();

        // ---- hotkeys ----
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::S)) {
            self.save_and_continue(ctx, false);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Enter)) {
            self.save_and_continue(ctx, true);
            return;
        }
        if !typing {
            for (i, key) in NUM_KEYS.iter().enumerate().take(self.candidates.len()) {
                if ctx.input_mut(|inp| inp.consume_key(Modifiers::NONE, *key)) {
                    self.choose_candidate(i);
                }
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::K)) {
                self.choose_keep();
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::D)) {
                self.choose_delete();
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::E)) {
                self.focus_editor = true;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::R)) {
                self.enter_unit(ctx);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::N)) {
                self.skip_unit(ctx);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::P)) {
                self.prev_unit(ctx);
                return;
            }
        }

        // ---- header ----
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{}:{}-{}", unit.file, unit.start_line, unit.end_line))
                    .monospace()
                    .strong(),
            );
            theme::badge(ui, &unit.lang, theme::ACCENT);
            if let Some(p) = &self.plan {
                let (fi, ft) = p.file_progress();
                ui.label(theme::dim(&format!("comment {}/{} in file", fi + 1, ft)));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(theme::dim(&crate::app::truncate(&unit.hunk_header, 70)));
            });
        });
        if let Some(err) = self.review_error.clone() {
            ui.colored_label(theme::BAD, err);
        }
        ui.add_space(2.0);

        let total_h = ui.available_height();

        // ---- context ----
        theme::section_title(ui, "CONTEXT");
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(8, 11, 15))
            .inner_margin(egui::Margin::same(4.0))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("context_scroll")
                    .max_height(total_h * 0.30)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for line in unit.context.lines() {
                            let marked = line.starts_with('>');
                            let rt = RichText::new(line).monospace();
                            if marked {
                                ui.label(
                                    rt.background_color(theme::MARK_BG)
                                        .color(egui::Color32::from_rgb(230, 237, 243)),
                                );
                            } else {
                                ui.label(rt.color(theme::TEXT_DIM));
                            }
                        }
                    });
            });
        ui.add_space(4.0);

        // ---- candidates ----
        theme::section_title(ui, "CANDIDATES");
        let n_slots = self.candidates.len().max(1);
        let mut pick: Option<usize> = None;
        ui.columns(n_slots, |cols| {
            for (i, ui) in cols.iter_mut().enumerate() {
                let name = self
                    .settings
                    .models
                    .get(i)
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| format!("model {i}"));
                let is_chosen = self.chosen == Some(Choice::Candidate(i));
                let color = theme::model_color(i);
                let frame = egui::Frame::group(ui.style())
                    .fill(if is_chosen { theme::RAISED } else { theme::PANEL })
                    .stroke(egui::Stroke::new(
                        if is_chosen { 2.0 } else { 1.0 },
                        if is_chosen { color } else { egui::Color32::from_gray(50) },
                    ))
                    .inner_margin(egui::Margin::same(6.0));
                frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("[{}]", i + 1)).monospace().strong().color(color),
                        );
                        ui.label(RichText::new(&name).strong().color(color));
                        match self.candidates.get(i) {
                            Some(CandidateState::Ready(s)) => {
                                theme::badge(ui, s.action.label(), theme::action_color(s.action));
                                ui.label(theme::dim(&format!("{} ms", s.latency_ms)));
                            }
                            Some(CandidateState::Pending) => {
                                ui.spinner();
                                ui.label(theme::dim("thinking…"));
                            }
                            Some(CandidateState::Failed(_)) => theme::badge(ui, "ERROR", theme::BAD),
                            _ => {
                                ui.label(theme::dim("disabled"));
                            }
                        }
                    });
                    match self.candidates.get(i) {
                        Some(CandidateState::Ready(s)) => {
                            ui.label(
                                RichText::new(format!("“{}”", s.justification))
                                    .small()
                                    .italics()
                                    .color(theme::TEXT_DIM),
                            );
                            egui::ScrollArea::vertical()
                                .id_salt(("cand", i))
                                .max_height(96.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    let preview = match s.action {
                                        Action::Keep => "(keep original text)".to_string(),
                                        Action::Delete => "(delete this comment)".to_string(),
                                        Action::Rewrite => s.comment.clone(),
                                    };
                                    ui.label(RichText::new(preview).monospace());
                                });
                            if ui
                                .add(egui::Button::new(
                                    RichText::new(format!("PICK [{}]", i + 1)).strong(),
                                ))
                                .clicked()
                            {
                                pick = Some(i);
                            }
                        }
                        Some(CandidateState::Failed(e)) => {
                            ui.label(RichText::new(crate::app::truncate(e, 220)).small().color(theme::BAD));
                        }
                        _ => {}
                    }
                });
            }
        });
        if let Some(i) = pick {
            self.choose_candidate(i);
        }
        ui.add_space(4.0);

        // ---- original vs final ----
        let editor_id = egui::Id::new("final_editor");
        let action = review::final_action(&self.editor, &self.original_display);
        let mut keep_clicked = false;
        ui.columns(2, |cols| {
            {
                let ui = &mut cols[0];
                ui.horizontal(|ui| {
                    theme::section_title(ui, "ORIGINAL");
                    if ui.small_button("keep [K]").clicked() {
                        keep_clicked = true;
                    }
                });
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(8, 11, 15))
                    .inner_margin(egui::Margin::same(4.0))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("orig_scroll")
                            .max_height(110.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.label(RichText::new(&self.original_display).monospace());
                            });
                    });
            }
            {
                let ui = &mut cols[1];
                ui.horizontal(|ui| {
                    theme::section_title(ui, "FINAL (editable)");
                    theme::badge(ui, action.label(), theme::action_color(action));
                });
                let mut editor = std::mem::take(&mut self.editor);
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut editor)
                        .id(editor_id)
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(5)
                        .desired_width(f32::INFINITY),
                );
                self.editor = editor;
                if self.focus_editor {
                    resp.request_focus();
                    self.focus_editor = false;
                }
            }
        });

        if keep_clicked {
            self.choose_keep();
        }

        // provenance preview
        let chosen_model = match &self.chosen {
            Some(Choice::Candidate(i)) => self
                .settings
                .models
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
        ui.horizontal(|ui| {
            ui.label(theme::dim("provenance:"));
            ui.label(RichText::new(provenance.source_str()).monospace().small());
            if let review::Provenance::Model { coauthor, .. } = &provenance {
                ui.label(theme::dim(&format!("· Co-authored-by: {coauthor}")));
            }
        });

        // ---- continuation buttons ----
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .button(RichText::new("💾 Save and Continue  [Ctrl+S]").strong())
                .clicked()
            {
                self.save_and_continue(ctx, false);
                return;
            }
            if ui
                .button(RichText::new("⎘ Commit and Continue  [Ctrl+Enter]").strong())
                .clicked()
            {
                self.save_and_continue(ctx, true);
                return;
            }
            ui.separator();
            if ui.button("Keep original [K]").clicked() {
                self.choose_keep();
            }
            if ui.button("Delete [D]").clicked() {
                self.choose_delete();
            }
            if ui.button("Re-run models [R]").clicked() {
                self.enter_unit(ctx);
                return;
            }
            if ui.button("◀ Prev [P]").clicked() {
                self.prev_unit(ctx);
                return;
            }
            if ui.button("Skip ▶ [N]").clicked() {
                self.skip_unit(ctx);
            }
        });
    }
}
