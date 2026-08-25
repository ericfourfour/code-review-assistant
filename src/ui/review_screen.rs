//! The one-comment-at-a-time review screen: context, three model candidates,
//! editable final text, save/commit continuations.

use egui::{Key, Modifiers, RichText};

use crate::app::{CandidateState, CraApp};
use crate::models::Action;
use crate::review::{self, Choice};
use crate::ui::{code, theme};

const NUM_KEYS: [Key; 9] = [
    Key::Num1,
    Key::Num2,
    Key::Num3,
    Key::Num4,
    Key::Num5,
    Key::Num6,
    Key::Num7,
    Key::Num8,
    Key::Num9,
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
        // An alias for Ctrl+S rather than a forced commit: the checkbox is
        // the only thing that decides that now.
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Enter)) {
            self.save_and_continue(ctx, false);
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
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::C)) {
                self.focus_note = true;
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
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::X)) {
                self.end_session();
                return;
            }
        }

        // ---- header ----
        let is_code = unit.is_code();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{}:{}-{}",
                    unit.file(),
                    unit.start_line(),
                    unit.end_line()
                ))
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
                RichText::new(format!("risk {}", risk.score))
                    .small()
                    .strong()
                    .color(risk_color),
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
            // A mismatch left resolution state behind: offer the exits right
            // here — re-review what the file holds now, or move on — instead
            // of stranding the review on an error it cannot get past.
            if let Some(stale) = &self.stale_unit {
                let mut reload = false;
                let mut skip = false;
                ui.horizontal(|ui| {
                    if ui
                        .button("Reload unit from disk")
                        .on_hover_text(
                            "Re-read the lines as they sit now and review them fresh — \
                             the models are asked again",
                        )
                        .clicked()
                    {
                        reload = true;
                    }
                    if ui
                        .button("Skip unit")
                        .on_hover_text("Leave the file alone and move on")
                        .clicked()
                    {
                        skip = true;
                    }
                    ui.label(theme::dim(&format!(
                        "on disk now at line {}:",
                        stale.start0 + 1
                    )));
                });
                egui::Frame::none()
                    .fill(theme::CODE_BG)
                    .inner_margin(egui::Margin::same(4.0))
                    .show(ui, |ui| {
                        let shown = 6.min(stale.lines.len());
                        for l in &stale.lines[..shown] {
                            ui.label(RichText::new(l).monospace().color(theme::CODE));
                        }
                        if stale.lines.len() > shown {
                            ui.label(theme::dim(&format!(
                                "… {} more line(s)",
                                stale.lines.len() - shown
                            )));
                        }
                        if stale.lines.is_empty() {
                            ui.label(theme::dim("nothing — the region was removed"));
                        }
                    });
                if reload {
                    self.reload_stale_unit(ctx);
                    return;
                }
                if skip {
                    self.skip_unit(ctx);
                    return;
                }
            }
        }
        ui.add_space(2.0);

        let total_h = ui.available_height();

        // ---- context ----
        theme::section_title(ui, "CONTEXT");
        let rows = context_rows(unit.context());
        egui::Frame::none()
            .fill(theme::CODE_BG)
            .inner_margin(egui::Margin::same(4.0))
            .show(ui, |ui| {
                egui::ScrollArea::both()
                    .id_salt("context_scroll")
                    .max_height(total_h * 0.30)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        code::show(ui, unit.file(), &rows);
                    });
            });
        ui.add_space(4.0);

        // ---- candidates ----
        theme::section_title(ui, "CANDIDATES");
        let model_count = self.candidates.len().max(1);
        let mut pick: Option<usize> = None;
        let mut ask_one: Option<usize> = None;
        let mut show_prompt: Option<usize> = None;
        let mut evidence_click: Option<crate::models::Evidence> = None;
        let can_ask_model: Vec<bool> = (0..model_count).map(|i| self.can_ask(i)).collect();
        let order = self.candidate_order();
        let hidden = self.names_hidden();
        ui.columns(model_count, |cols| {
            for (pos, ui) in cols.iter_mut().enumerate() {
                // `pos` is where the card sits on screen; `i` is which model_index it
                // actually belongs to. They differ while blinding is on.
                let i = order.get(pos).copied().unwrap_or(pos);
                let can_ask = can_ask_model.get(i).copied().unwrap_or(false);
                let name = self.model_label(i, pos);
                let is_chosen = self.chosen == Some(Choice::Candidate(i));
                // Colour is per display position while hidden, so the palette
                // does not give the model away either.
                let color = theme::model_color(if hidden { pos } else { i });
                let frame = egui::Frame::group(ui.style())
                    .fill(if is_chosen {
                        theme::RAISED
                    } else {
                        theme::PANEL
                    })
                    .stroke(egui::Stroke::new(
                        if is_chosen { 2.0_f32 } else { 1.0_f32 },
                        if is_chosen {
                            color
                        } else {
                            egui::Color32::from_gray(50)
                        },
                    ))
                    .inner_margin(egui::Margin::same(6.0));
                frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("[{}]", pos + 1))
                                .monospace()
                                .strong()
                                .color(color),
                        );
                        ui.label(RichText::new(&name).strong().color(color));
                        if !hidden {
                            if let Some(variant) = self
                                .candidate_models
                                .get(i)
                                .map(|m| m.model.trim())
                                .filter(|v| !v.is_empty())
                            {
                                ui.label(
                                    RichText::new(variant)
                                        .monospace()
                                        .small()
                                        .color(theme::TEXT_DIM),
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
                            Some(CandidateState::Pending(live)) => {
                                ui.spinner();
                                let snap = live.snapshot();
                                ui.label(theme::dim(&format!(
                                    "thinking… {}",
                                    snap.clock(self.settings.model_timeout_secs)
                                )));
                                if let Some(a) = snap.activity_line() {
                                    ui.label(theme::dim(&a));
                                }
                            }
                            Some(CandidateState::Failed(_)) => {
                                theme::badge(ui, "ERROR", theme::BAD)
                            }
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
                            egui::ScrollArea::both()
                                .id_salt(("cand", i))
                                .max_height(96.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    // A rewrite previews as the text picking it
                                    // would put in the editor — real code, not the
                                    // model's raw prose. Code shows it as a diff
                                    // against the original: a REVISE is a claim
                                    // about what should change, and a fresh block
                                    // leaves the eye to find that change by hand.
                                    // Comments do not — prose rewords wholesale,
                                    // so a line diff there is only the old text
                                    // restated above the new.
                                    let stand_in = match (s.action, is_code) {
                                        (Action::Keep, false) => "(keep original text)",
                                        (Action::Keep, true) => "(approve — sound as written)",
                                        (Action::Delete, false) => "(delete this comment)",
                                        (Action::Delete, true) => "(delete these lines)",
                                        (Action::Flag, _) => {
                                            "(flagged — no replacement proposed; \
                                                              the concern above is the verdict)"
                                        }
                                        (Action::Rewrite, _) => "",
                                    };
                                    if stand_in.is_empty() {
                                        let text = unit.replacement_display(&s.comment);
                                        if is_code {
                                            code::show(
                                                ui,
                                                unit.file(),
                                                &code::diff_rows(&unit.display_text(), &text),
                                            );
                                        } else {
                                            code::show_block(ui, unit.file(), &text, theme::CODE);
                                        }
                                    } else {
                                        ui.label(
                                            RichText::new(stand_in)
                                                .monospace()
                                                .color(theme::TEXT_DIM),
                                        );
                                    }
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
                                if ui
                                    .button("🗎")
                                    .on_hover_text("show the prompt/transcript")
                                    .clicked()
                                {
                                    show_prompt = Some(i);
                                }
                            });
                        }
                        Some(CandidateState::Failed(e)) => {
                            ui.label(RichText::new(crate::app::truncate(e, 220)).color(theme::BAD));
                            ui.horizontal(|ui| {
                                if ui
                                    .add_enabled(can_ask, egui::Button::new("↩ ask"))
                                    .clicked()
                                {
                                    ask_one = Some(i);
                                }
                                if ui
                                    .button("🗎")
                                    .on_hover_text("show the prompt/transcript")
                                    .clicked()
                                {
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
                    (0..model_count).any(|i| can_ask_model.get(i).copied().unwrap_or(false)),
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
            let submitted =
                resp.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift);
            ask_all = send_all || submitted;
        });
        if let Some(i) = ask_one {
            self.ask_followup(ctx, Some(i));
        } else if ask_all {
            self.ask_followup(ctx, None);
        }

        // ---- note for follow-up ----
        //
        // A unit sometimes reveals an issue bigger than itself, and the
        // review flow deliberately cannot act beyond the unit's own lines.
        // The note parks that issue for the follow-up screen; the unit still
        // gets decided here, on its own merits.
        let mut leave = false;
        ui.horizontal(|ui| {
            theme::section_title(ui, "NOTE");
            leave = ui
                .add_enabled(
                    !self.note_input.trim().is_empty(),
                    egui::Button::new("Park for follow-up  [Enter]"),
                )
                .on_hover_text(
                    "saved with this unit's file, lines and code — triage it later on the \
                     follow-up screen, where a model with room for bigger changes takes over",
                )
                .clicked();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.note_input)
                    .id(egui::Id::new("note_box"))
                    .desired_width(f32::INFINITY)
                    .hint_text(
                        "issue bigger than this unit? note it for the follow-up session — \
                         this unit still gets decided here",
                    ),
            );
            if self.focus_note {
                resp.request_focus();
                self.focus_note = false;
            }
            let submitted =
                resp.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift);
            leave = leave || submitted;
        });
        if leave {
            self.leave_note();
        }
        ui.add_space(4.0);

        // ---- original vs final ----
        //
        // The two panes take every pixel the rows under them do not, and both
        // scroll in both directions: a unit is as long as it is, and a
        // fixed-height box either wastes the space below it or hides the text
        // that ran past it.
        let editor_id = egui::Id::new("final_editor");
        let action = self.current_action();
        let mut keep_clicked = false;
        // Everything from here down that is not the panes themselves — their
        // headers and frame margins, the provenance and commit rows, the
        // button bar and the spacing between all of it — is measured on the
        // way past rather than estimated. Estimating it ran a few pixels
        // light and clipped the button bar off the bottom of the window.
        let overhead_id = egui::Id::new("review_pane_overhead");
        let overhead = ui
            .ctx()
            .memory(|m| m.data.get_temp(overhead_id))
            .unwrap_or(FIRST_FRAME_OVERHEAD);
        let panes_h = pane_height(ui.available_height(), overhead);
        let measured_from = ui.cursor().top();
        let path = unit.file().to_string();
        ui.columns(2, |cols| {
            {
                let ui = &mut cols[0];
                ui.horizontal(|ui| {
                    theme::section_title(ui, "ORIGINAL");
                    let keep_label = if is_code {
                        "approve as-is [K]"
                    } else {
                        "keep [K]"
                    };
                    if ui.small_button(keep_label).clicked() {
                        keep_clicked = true;
                    }
                });
                egui::Frame::none()
                    .fill(theme::CODE_BG)
                    .inner_margin(egui::Margin::same(4.0))
                    .show(ui, |ui| {
                        egui::ScrollArea::both()
                            .id_salt("orig_scroll")
                            .max_height(panes_h)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                code::show_block(ui, &path, &self.original_display, theme::CODE);
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
                let mono_h = ui.text_style_height(&egui::TextStyle::Monospace);
                let rows = (panes_h / mono_h).floor().max(3.0) as usize;
                let mut layouter = |ui: &egui::Ui, text: &str, _wrap: f32| {
                    code::galley(ui, &path, &code::rows(text, theme::CODE))
                };
                let resp = egui::ScrollArea::both()
                    .id_salt("editor_scroll")
                    .max_height(panes_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut editor)
                                .id(editor_id)
                                .font(egui::TextStyle::Monospace)
                                .desired_rows(rows)
                                .desired_width(f32::INFINITY)
                                .layouter(&mut layouter),
                        )
                    })
                    .inner;
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
                if !coauthor.trim().is_empty() {
                    ui.label(theme::dim(&format!("· Co-authored-by: {coauthor}")));
                }
            }
        });

        // ---- commit style ----
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.commit_each, "Commit each decision individually");
            ui.label(theme::dim(if self.commit_each {
                "— every decision is its own commit, made immediately"
            } else {
                "— decisions are only written to the working tree; commit them \
                 with git when ready"
            }));
        });

        // ---- continuation ----
        // One button: the checkbox above already decides whether continuing
        // also commits, so a second button could only contradict it.
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let continue_label = if self.commit_each {
                "⎘ Commit and Continue  [Ctrl+S]"
            } else {
                "💾 Save and Continue  [Ctrl+S]"
            };
            if ui.button(RichText::new(continue_label).strong()).clicked() {
                self.save_and_continue(ctx, false);
                return;
            }
            ui.separator();
            if ui
                .button(if is_code {
                    "Approve as-is [K]"
                } else {
                    "Keep original [K]"
                })
                .clicked()
            {
                self.choose_keep();
            }
            if ui
                .button(if is_code {
                    "Delete lines [D]"
                } else {
                    "Delete [D]"
                })
                .clicked()
            {
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
            ui.separator();
            if ui
                .button("■ End session [X]")
                .on_hover_text(
                    "go to the summary without finishing the review — decisions made so far \
                    stand, the rest resume from the file picker, and the whole-branch review and \
                     follow-up notes are available from there",
                )
                .clicked()
            {
                self.end_session();
            }
        });

        // What all of that actually cost, minus the panes, for the next
        // frame to size them by. The floating windows below allocate nothing
        // here, so this is the whole screen.
        let spent = ui.cursor().top() - measured_from - panes_h;
        ui.ctx()
            .memory_mut(|m| m.data.insert_temp(overhead_id, spent.clamp(40.0, 600.0)));

        self.prompt_window(ctx);
        self.evidence_window(ctx);
    }

    /// Floating viewer for one evidence entry: the real file at the spot a
    /// model says it read, so the human can weigh the verdict against the
    /// same context — not the model's paraphrase of it.
    pub(crate) fn evidence_window(&mut self, ctx: &egui::Context) {
        let Some(ev) = self.show_evidence.clone() else {
            return;
        };
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
                            format!(
                                "could not read {} — the model may have misreported it ({e})",
                                ev.file
                            ),
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
                // A margin around the named range keeps it honest: the reader
                // sees what surrounds the excerpt the model chose.
                let view_lo = lo.saturating_sub(8);
                let view_hi = (hi + 8).min(lines.len().saturating_sub(1));
                let rows: Vec<code::Line> = lines
                    .iter()
                    .enumerate()
                    .take(view_hi + 1)
                    .skip(view_lo)
                    .map(|(i, line)| {
                        let in_range = i >= lo && i <= hi;
                        let base = if in_range {
                            theme::CODE
                        } else {
                            theme::CODE_CTX
                        };
                        let row = code::Line::code(*line, base)
                            .gutter(format!("{:>5}| ", i + 1), theme::GUTTER);
                        if in_range {
                            row.background(theme::MARK_BG)
                        } else {
                            row
                        }
                    })
                    .collect();
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        code::show(ui, &ev.file, &rows);
                    });
            });
        if !open {
            self.show_evidence = None;
        }
    }

    /// Floating inspector for exactly what was piped to a model CLI and what
    /// it printed back, including every follow-up turn.
    fn prompt_window(&mut self, ctx: &egui::Context) {
        let Some(model_index) = self.show_prompt else {
            return;
        };
        let name = self
            .settings
            .models
            .get(model_index)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| format!("model {model_index}"));
        let turns = self.convos.get(model_index).cloned().unwrap_or_default();
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
                    match self.sessions.get(model_index).and_then(|s| s.clone()) {
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
                                    // The running turn: the same live view
                                    // the candidate row shows.
                                    if let Some(CandidateState::Pending(live)) =
                                        self.candidates.get(model_index)
                                    {
                                        let snap = live.snapshot();
                                        ui.label(theme::dim(&format!(
                                            "waiting… {}",
                                            snap.clock(self.settings.model_timeout_secs)
                                        )));
                                        if let Some(a) = snap.activity_line() {
                                            ui.label(theme::dim(&a));
                                        }
                                    } else {
                                        ui.label(theme::dim("waiting…"));
                                    }
                                });
                            } else {
                                ui.label(
                                    RichText::new(&t.reply).monospace().color(theme::TEXT_DIM),
                                );
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

/// What to assume the pane overhead is before a frame has measured it.
/// Deliberately generous: too large only makes the first frame's panes short,
/// where too small would let them push the buttons off the screen — and the
/// real figure arrives before anyone can read either.
const FIRST_FRAME_OVERHEAD: f32 = 160.0;

/// How much height the ORIGINAL and FINAL panes get: whatever is left once
/// everything else on the screen below the candidates is paid for, and never
/// less than a workable minimum on a window too short to satisfy.
fn pane_height(available: f32, overhead: f32) -> f32 {
    (available - overhead).max(110.0)
}

/// Turn a rendered excerpt back into display rows. Every line is
/// `{marker}{line number}| {code}`: the marker and number become the gutter,
/// the code keeps its own indentation and gets syntax colour, and the diff
/// role shows as a background tint so colour is free to mean syntax.
///
/// Removed lines are marked unparsed — they are not in the file the grammar
/// is being asked about, and mixing them in would only confuse the parse.
fn context_rows(context: &str) -> Vec<code::Line> {
    context
        .lines()
        .map(|line| {
            // The renderer puts exactly one space after the bar; anything
            // beyond it is the line's own indentation. A line that does not
            // fit the shape is all code.
            let split = line
                .find('|')
                .map(|i| i + 2)
                .filter(|p| *p <= line.len() && line.is_char_boundary(*p));
            let (gutter, text) = match split {
                Some(p) => line.split_at(p),
                None => ("", line),
            };
            let marker = line.chars().next().unwrap_or(' ');
            let (base, bg) = match marker {
                '>' => (theme::CODE, Some(theme::MARK_BG)),
                '+' => (theme::CODE, Some(theme::ADD_BG)),
                '-' => (theme::REMOVED, Some(theme::DEL_BG)),
                _ => (theme::CODE_CTX, None),
            };
            let gutter_color = match marker {
                '+' => theme::ADDED,
                '-' => theme::REMOVED,
                _ => theme::GUTTER,
            };
            let mut row = code::Line::code(text, base).gutter(gutter, gutter_color);
            if let Some(bg) = bg {
                row = row.background(bg);
            }
            if marker == '-' {
                row = row.unparsed();
            }
            row
        })
        .collect()
}

/// Parse a model-reported line range ("12-40", "12", "L12-L40") into 0-based
/// inclusive bounds, clamped to the file. Unparsable ranges show the top of
/// the file rather than nothing — a wrong spot the human can see beats an
/// error they have to interpret.
fn evidence_range(spec: &str, len: usize) -> (usize, usize) {
    let last = len.saturating_sub(1);
    let parse = |s: &str| {
        s.trim()
            .trim_start_matches(['L', 'l'])
            .parse::<usize>()
            .ok()
    };
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
    use super::{context_rows, evidence_range, pane_height};
    use crate::ui::theme;

    #[test]
    fn context_rows_split_the_gutter_off_and_keep_the_indentation() {
        let rendered = concat!(
            "    11|     let a = 1;\n",
            ">   12|         deeper();\n",
            "-     |         gone();\n",
            "+   13|     let b = 2;\n",
        );
        let rows = context_rows(rendered);
        assert_eq!(rows.len(), 4);
        // The code keeps every leading space it had in the file.
        assert_eq!(rows[0].text, "    let a = 1;");
        assert_eq!(rows[1].text, "        deeper();");
        assert_eq!(rows[0].gutter, "    11| ");
        assert_eq!(rows[1].gutter, ">   12| ");
        // Diff role lives in the background, leaving colour free for syntax.
        assert_eq!(rows[1].background, Some(theme::MARK_BG));
        assert_eq!(rows[2].background, Some(theme::DEL_BG));
        assert_eq!(rows[3].background, Some(theme::ADD_BG));
        assert!(rows[0].background.is_none());
        // A removed line is not in the file, so it is not in the parse.
        assert!(!rows[2].syntax, "removed lines must stay out of the parse");
        assert!(rows.iter().enumerate().all(|(i, r)| i == 2 || r.syntax));
    }

    #[test]
    fn a_line_with_no_gutter_is_all_code() {
        let rows = context_rows("no gutter here\n");
        assert_eq!(rows[0].gutter, "");
        assert_eq!(rows[0].text, "no gutter here");
    }

    #[test]
    fn the_panes_take_the_space_left_over_but_never_collapse() {
        // Every pixel the rest of the screen does not need goes to the panes.
        assert_eq!(pane_height(600.0, 95.0), 505.0);
        assert_eq!(pane_height(900.0, 95.0), 805.0);
        // A window too short to satisfy still leaves something usable rather
        // than a zero-height box.
        assert_eq!(pane_height(40.0, 95.0), 110.0);
    }

    #[test]
    fn evidence_ranges_parse_and_clamp() {
        assert_eq!(evidence_range("12-40", 100), (11, 39));
        assert_eq!(evidence_range("12", 100), (11, 11));
        assert_eq!(evidence_range("L12-L40", 100), (11, 39));
        assert_eq!(
            evidence_range("40-12", 100),
            (39, 39),
            "a backwards range never inverts"
        );
        assert_eq!(
            evidence_range("90-200", 100),
            (89, 99),
            "clamped to the file"
        );
        assert_eq!(evidence_range("", 100), (0, 39), "no range shows the top");
        assert_eq!(evidence_range("garbage", 10), (0, 9));
    }
}
