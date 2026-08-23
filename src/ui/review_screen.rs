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
            let order = self.candidate_order();
            for (pos, key) in NUM_KEYS.iter().enumerate().take(order.len()) {
                if ctx.input_mut(|inp| inp.consume_key(Modifiers::NONE, *key)) {
                    self.choose_candidate(order[pos]);
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
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F)) {
                self.focus_follow_up = true;
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
        let is_code = unit.is_code();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{}:{}-{}", unit.file(), unit.start_line(), unit.end_line()))
                    .monospace()
                    .strong(),
            );
            theme::badge(ui, unit.lang(), theme::ACCENT);
            theme::badge(
                ui,
                &unit.kind_label(),
                if is_code { theme::WARN } else { theme::ACCENT },
            );
            let risk = crate::triage::assess(&unit);
            let risk_color = if risk.score >= 60 {
                theme::BAD
            } else if risk.score >= 30 {
                theme::WARN
            } else {
                egui::Color32::from_gray(120)
            };
            ui.label(
                RichText::new(format!("risk {}", risk.score)).small().strong().color(risk_color),
            )
            .on_hover_text(risk.reasons.join("\n"));
            if let Some(p) = &self.plan {
                let (fi, ft) = p.file_progress();
                ui.label(theme::dim(&format!("unit {}/{} in file", fi + 1, ft)));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(theme::dim(&crate::app::truncate(unit.hunk_header(), 70)));
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
                        for line in unit.context().lines() {
                            let rt = RichText::new(line).monospace();
                            match line.chars().next() {
                                Some('>') => {
                                    ui.label(
                                        rt.background_color(theme::MARK_BG)
                                            .color(egui::Color32::from_rgb(230, 237, 243)),
                                    );
                                }
                                // Removed by this change — shown where it was.
                                Some('-') => {
                                    ui.label(rt.color(egui::Color32::from_rgb(190, 100, 95)));
                                }
                                // Added by this branch, outside this unit.
                                Some('+') => {
                                    ui.label(rt.color(egui::Color32::from_rgb(150, 190, 150)));
                                }
                                _ => {
                                    ui.label(rt.color(theme::TEXT_DIM));
                                }
                            }
                        }
                    });
            });
        ui.add_space(4.0);

        // ---- candidates ----
        theme::section_title(ui, "CANDIDATES");
        let n_slots = self.candidates.len().max(1);
        let mut pick: Option<usize> = None;
        let mut ask_one: Option<usize> = None;
        let mut show_prompt: Option<usize> = None;
        let mut evidence_click: Option<crate::models::Evidence> = None;
        let can_ask_slot: Vec<bool> = (0..n_slots).map(|i| self.can_ask(i)).collect();
        let order = self.candidate_order();
        let hidden = self.names_hidden();
        ui.columns(n_slots, |cols| {
            for (pos, ui) in cols.iter_mut().enumerate() {
                // `pos` is where the card sits on screen; `i` is which slot it
                // actually belongs to. They differ while blinding is on.
                let i = order.get(pos).copied().unwrap_or(pos);
                let can_ask = can_ask_slot.get(i).copied().unwrap_or(false);
                let name = self.slot_label(i, pos);
                let is_chosen = self.chosen == Some(Choice::Candidate(i));
                // Colour is per display position while hidden, so the palette
                // does not give the model away either.
                let color = theme::model_color(if hidden { pos } else { i });
                let frame = egui::Frame::group(ui.style())
                    .fill(if is_chosen { theme::RAISED } else { theme::PANEL })
                    .stroke(egui::Stroke::new(
                        if is_chosen { 2.0_f32 } else { 1.0_f32 },
                        if is_chosen { color } else { egui::Color32::from_gray(50) },
                    ))
                    .inner_margin(egui::Margin::same(6.0));
                frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("[{}]", pos + 1)).monospace().strong().color(color),
                        );
                        ui.label(RichText::new(&name).strong().color(color));
                        if !hidden {
                            if let Some(variant) = self
                                .settings
                                .models
                                .get(i)
                                .map(|m| m.model.trim())
                                .filter(|v| !v.is_empty())
                            {
                                ui.label(
                                    RichText::new(variant).monospace().small().color(theme::TEXT_DIM),
                                );
                            }
                            if let Some(Some(id)) = self.sessions.get(i) {
                                ui.label(theme::dim(&format!("· {}", &id[..8.min(id.len())])))
                                    .on_hover_text(format!("session {id}"));
                            }
                        }
                        match self.candidates.get(i) {
                            Some(CandidateState::Ready(s)) => {
                                theme::badge(
                                    ui,
                                    crate::units::action_label(s.action, unit.kind()),
                                    theme::action_color(s.action),
                                );
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
                                    .italics()
                                    .color(egui::Color32::from_rgb(196, 208, 220)),
                            );
                            egui::ScrollArea::vertical()
                                .id_salt(("cand", i))
                                .max_height(96.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    let preview = match (s.action, is_code) {
                                        (Action::Keep, false) => "(keep original text)".to_string(),
                                        (Action::Keep, true) => "(approve — sound as written)".to_string(),
                                        (Action::Delete, false) => "(delete this comment)".to_string(),
                                        (Action::Delete, true) => "(delete these lines)".to_string(),
                                        (Action::Flag, _) => "(flagged — no replacement proposed; \
                                                              the concern above is the verdict)"
                                            .to_string(),
                                        (Action::Rewrite, _) => s.comment.clone(),
                                    };
                                    ui.label(RichText::new(preview).monospace());
                                });
                            // What the model says it read on the way to this
                            // verdict — click to see that spot in the real file.
                            if !s.evidence.is_empty() {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(theme::dim("read:"));
                                    for ev in &s.evidence {
                                        let label = if ev.lines.trim().is_empty() {
                                            ev.file.clone()
                                        } else {
                                            format!("{}:{}", ev.file, ev.lines)
                                        };
                                        let resp = ui.small_button(
                                            RichText::new(label).monospace().small(),
                                        );
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
                            ui.horizontal(|ui| {
                                if ui
                                    .add(egui::Button::new(
                                        RichText::new(format!("PICK [{}]", pos + 1)).strong(),
                                    ))
                                    .clicked()
                                {
                                    pick = Some(i);
                                }
                                if ui
                                    .add_enabled(can_ask, egui::Button::new("↩ ask"))
                                    .on_hover_text("send the follow-up below to this model only")
                                    .clicked()
                                {
                                    ask_one = Some(i);
                                }
                                if ui.button("🗎").on_hover_text("show the prompt/transcript").clicked() {
                                    show_prompt = Some(i);
                                }
                            });
                        }
                        Some(CandidateState::Failed(e)) => {
                            ui.label(RichText::new(crate::app::truncate(e, 220)).color(theme::BAD));
                            ui.horizontal(|ui| {
                                if ui.add_enabled(can_ask, egui::Button::new("↩ ask")).clicked() {
                                    ask_one = Some(i);
                                }
                                if ui.button("🗎").on_hover_text("show the prompt/transcript").clicked() {
                                    show_prompt = Some(i);
                                }
                            });
                        }
                        _ => {}
                    }
                });
            }
        });
        if let Some(i) = pick {
            self.choose_candidate(i);
        }
        if let Some(i) = show_prompt {
            self.show_prompt = Some(i);
        }
        if let Some(ev) = evidence_click {
            self.show_evidence = Some(ev);
        }

        // ---- follow-up ----
        let mut ask_all = false;
        ui.horizontal(|ui| {
            theme::section_title(ui, "FOLLOW-UP");
            let send_all = ui
                .add_enabled(
                    (0..n_slots).any(|i| can_ask_slot.get(i).copied().unwrap_or(false)),
                    egui::Button::new("Send to all  [Enter]"),
                )
                .clicked();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.follow_up)
                    .id(egui::Id::new("follow_up_box"))
                    .desired_width(f32::INFINITY)
                    .hint_text("ask the models to reconsider, e.g. “too wordy — one line?”"),
            );
            if self.focus_follow_up {
                resp.request_focus();
                self.focus_follow_up = false;
            }
            let submitted = resp.lost_focus()
                && ctx.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift);
            ask_all = send_all || submitted;
        });
        if let Some(i) = ask_one {
            self.ask_followup(ctx, Some(i));
        } else if ask_all {
            self.ask_followup(ctx, None);
        }
        ui.add_space(4.0);

        // ---- original vs final ----
        let editor_id = egui::Id::new("final_editor");
        let action = self.current_action();
        let mut keep_clicked = false;
        ui.columns(2, |cols| {
            {
                let ui = &mut cols[0];
                ui.horizontal(|ui| {
                    theme::section_title(ui, "ORIGINAL");
                    let keep_label = if is_code { "approve as-is [K]" } else { "keep [K]" };
                    if ui.small_button(keep_label).clicked() {
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
                    theme::badge(
                        ui,
                        crate::units::action_label(action, unit.kind()),
                        theme::action_color(action),
                    );
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

        // ---- commit style ----
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.commit_each, "Commit each decision individually");
            ui.label(theme::dim(if self.commit_each {
                "— every decision is its own commit, made immediately"
            } else {
                "— decisions batch uncommitted until Commit and Continue, which \
                 documents all of them in one commit"
            }));
        });

        // ---- continuation buttons ----
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let save_label = if self.commit_each {
                "💾 Save and Commit  [Ctrl+S]"
            } else {
                "💾 Save and Continue  [Ctrl+S]"
            };
            if ui.button(RichText::new(save_label).strong()).clicked() {
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
            if ui.button(if is_code { "Approve as-is [K]" } else { "Keep original [K]" }).clicked() {
                self.choose_keep();
            }
            if ui.button(if is_code { "Delete lines [D]" } else { "Delete [D]" }).clicked() {
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

        self.prompt_window(ctx);
        self.evidence_window(ctx);
    }

    /// Floating viewer for one evidence entry: the real file at the spot a
    /// model says it read, so the human can weigh the verdict against the
    /// same context — not the model's paraphrase of it.
    pub(crate) fn evidence_window(&mut self, ctx: &egui::Context) {
        let Some(ev) = self.show_evidence.clone() else { return };
        let mut open = true;
        egui::Window::new(format!("evidence · {}", ev.file))
            .id(egui::Id::new("evidence_window"))
            .open(&mut open)
            .default_size([760.0, 460.0])
            .collapsible(false)
            .show(ctx, |ui| {
                if !ev.note.trim().is_empty() {
                    ui.label(RichText::new(format!("“{}”", ev.note.trim())).italics());
                    ui.separator();
                }
                let Some(repo) = self.repo.as_ref().map(|r| r.path.clone()) else {
                    ui.label(theme::dim("no repository open"));
                    return;
                };
                let path = std::path::Path::new(&repo).join(&ev.file);
                let content = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        // Self-reported paths can be wrong — that is itself
                        // worth knowing when weighing the verdict.
                        ui.colored_label(
                            theme::BAD,
                            format!("could not read {} — the model may have misreported it ({e})", ev.file),
                        );
                        return;
                    }
                };
                let lines: Vec<&str> = content.lines().collect();
                if lines.is_empty() {
                    ui.label(theme::dim("(empty file)"));
                    return;
                }
                let (lo, hi) = evidence_range(&ev.lines, lines.len());
                ui.label(theme::dim(&format!(
                    "{} — lines {}-{} of {}",
                    ev.file,
                    lo + 1,
                    hi + 1,
                    lines.len()
                )));
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    // A margin around the named range keeps it honest: the
                    // reader sees what surrounds the excerpt the model chose.
                    let view_lo = lo.saturating_sub(8);
                    let view_hi = (hi + 8).min(lines.len().saturating_sub(1));
                    for (i, line) in lines.iter().enumerate().take(view_hi + 1).skip(view_lo) {
                        let in_range = i >= lo && i <= hi;
                        let rt = RichText::new(format!("{:>5}| {line}", i + 1)).monospace();
                        if in_range {
                            ui.label(rt.background_color(theme::MARK_BG));
                        } else {
                            ui.label(rt.color(theme::TEXT_DIM));
                        }
                    }
                });
            });
        if !open {
            self.show_evidence = None;
        }
    }

    /// Floating inspector for exactly what was piped to a model CLI and what
    /// it printed back, including every follow-up turn.
    fn prompt_window(&mut self, ctx: &egui::Context) {
        let Some(slot) = self.show_prompt else { return };
        let name = self
            .settings
            .models
            .get(slot)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| format!("model {slot}"));
        let turns = self.convos.get(slot).cloned().unwrap_or_default();
        let mut open = true;
        egui::Window::new(format!("prompt · {name}"))
            .id(egui::Id::new("prompt_window"))
            .open(&mut open)
            .default_size([760.0, 520.0])
            .collapsible(false)
            .show(ctx, |ui| {
                if turns.is_empty() {
                    ui.label(theme::dim("nothing sent yet"));
                    return;
                }
                ui.horizontal(|ui| {
                    match self.sessions.get(slot).and_then(|s| s.clone()) {
                        Some(id) => {
                            ui.label(theme::dim(&format!("{} turn(s) · session", turns.len())));
                            ui.label(RichText::new(&id).monospace().small());
                            if ui.small_button("copy id").clicked() {
                                ctx.copy_text(id.clone());
                            }
                        }
                        None => {
                            ui.label(theme::dim(&format!("{} turn(s) · no session", turns.len())));
                        }
                    }
                    if ui.button("copy last prompt").clicked() {
                        ctx.copy_text(turns.last().map(|t| t.prompt.clone()).unwrap_or_default());
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for (i, t) in turns.iter().enumerate() {
                            theme::section_title(ui, &format!("SENT — turn {}", i + 1));
                            ui.label(RichText::new(&t.prompt).monospace());
                            ui.add_space(4.0);
                            theme::section_title(ui, &format!("RECEIVED — turn {}", i + 1));
                            if t.reply.is_empty() {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(theme::dim("waiting…"));
                                });
                            } else {
                                ui.label(RichText::new(&t.reply).monospace().color(theme::TEXT_DIM));
                            }
                            ui.add_space(8.0);
                        }
                    });
            });
        if !open {
            self.show_prompt = None;
        }
    }
}

/// Parse a model-reported line range ("12-40", "12", "L12-L40") into 0-based
/// inclusive bounds, clamped to the file. Unparsable ranges show the top of
/// the file rather than nothing — a wrong spot the human can see beats an
/// error they have to interpret.
fn evidence_range(spec: &str, len: usize) -> (usize, usize) {
    let last = len.saturating_sub(1);
    let parse = |s: &str| s.trim().trim_start_matches(['L', 'l']).parse::<usize>().ok();
    let spec = spec.trim();
    let (a, b) = match spec.split_once(['-', ':']) {
        Some((a, b)) => (parse(a), parse(b)),
        None => (parse(spec), parse(spec)),
    };
    match (a, b) {
        (Some(a), Some(b)) if a >= 1 => ((a - 1).min(last), (b.max(a) - 1).min(last)),
        _ => (0, last.min(39)),
    }
}

#[cfg(test)]
mod tests {
    use super::evidence_range;

    #[test]
    fn evidence_ranges_parse_and_clamp() {
        assert_eq!(evidence_range("12-40", 100), (11, 39));
        assert_eq!(evidence_range("12", 100), (11, 11));
        assert_eq!(evidence_range("L12-L40", 100), (11, 39));
        assert_eq!(evidence_range("40-12", 100), (39, 39), "a backwards range never inverts");
        assert_eq!(evidence_range("90-200", 100), (89, 99), "clamped to the file");
        assert_eq!(evidence_range("", 100), (0, 39), "no range shows the top");
        assert_eq!(evidence_range("garbage", 10), (0, 9));
    }
}
