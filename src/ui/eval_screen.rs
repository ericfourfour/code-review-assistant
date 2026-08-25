//! The evaluation page: which model's suggestions you actually take, and what
//! each one costs you.
//!
//! Every review is a blind side-by-side test that already happened. Picking
//! one model's text over another's is a label, and this screen is that pile of
//! labels added up — win rate first, because a suggestion you took is the only
//! outcome the tool exists to produce, then agreement, then price.
//!
//! Three rules the layout follows, because the numbers here are small enough
//! to mislead easily:
//!
//! * **Every rate carries its denominator.** `62% (13/21)`, never `62%`. A
//!   rate over five contests and a rate over five hundred look identical
//!   otherwise, and the first one means nothing.
//! * **Colour identifies a model, never a rank.** A model configuration's colour is fixed in
//!   settings, so re-sorting the table or filtering to one repository never
//!   repaints a model into someone else's hue.
//! * **Unmeasured is not zero.** A CLI that reports no tokens shows a dash and
//!   is named in the caveats, rather than sitting at the top of the cost table
//!   looking free.

use egui::{Key, Modifiers, RichText};

use crate::app::{CraApp, Screen};
use crate::eval::{Filter, HeadToHead, Leaderboard, Standing};
use crate::ui::theme;

/// Width of the bar in the leaderboard's rate columns.
const BAR_W: f32 = 116.0;
const BAR_H: f32 = 9.0;

impl CraApp {
    pub fn ui_eval(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        if !ctx.wants_keyboard_input() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::R)) {
                self.reload_eval();
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::B)) {
                self.eval_filter.blinded_only = !self.eval_filter.blinded_only;
                self.reload_eval();
            }
        }
        if self.eval.is_none() {
            self.reload_eval();
        }

        ui.horizontal(|ui| {
            ui.heading("Model evaluation");
            ui.label(theme::dim(
                "— which suggestions you took, and what they cost",
            ));
        });
        ui.add_space(2.0);
        self.filter_row(ui);
        ui.add_space(6.0);

        let Some(board) = self.eval.take() else {
            return;
        };
        egui::ScrollArea::vertical()
            .id_salt("eval_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.headline(ui, &board);
                ui.add_space(10.0);
                self.leaderboard(ui, &board);
                ui.add_space(12.0);
                self.head_to_head(ui, &board);
                ui.add_space(12.0);
                self.verdict_mix(ui, &board);
                ui.add_space(12.0);
                self.spend(ui, &board);
                ui.add_space(12.0);
                caveats(ui, &board);
                ui.add_space(8.0);
            });
        self.eval = Some(board);
    }

    /// Scope controls. One row, above everything they affect, so the reading
    /// of every number below is set before any of them is read.
    fn filter_row(&mut self, ui: &mut egui::Ui) {
        let before = self.eval_filter.clone();
        ui.horizontal(|ui| {
            ui.label(theme::dim("repository"));
            let label = match &self.eval_filter.repo {
                Some(path) => short_repo(path),
                None => "all repositories".to_string(),
            };
            egui::ComboBox::from_id_salt("eval_repo")
                .selected_text(label)
                .width(240.0)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.eval_filter.repo.is_none(), "all repositories")
                        .clicked()
                    {
                        self.eval_filter.repo = None;
                    }
                    for repo in &self.eval_repos {
                        let on = self.eval_filter.repo.as_deref() == Some(repo.as_str());
                        if ui
                            .selectable_label(on, short_repo(repo))
                            .on_hover_text(repo)
                            .clicked()
                        {
                            self.eval_filter.repo = Some(repo.clone());
                        }
                    }
                });
            ui.separator();
            ui.checkbox(
                &mut self.eval_filter.blinded_only,
                "blinded decisions only [B]",
            )
            .on_hover_text(
                "A choice made while the model names were visible measures which model \
                     you already trust as much as it measures the suggestion. Off includes \
                     those; the caveats below say how many they are.",
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("↻ refresh [R]").clicked() {
                    self.eval = None;
                }
                if ui.button("⧉ copy as markdown").clicked() {
                    if let Some(board) = &self.eval {
                        ui.ctx().copy_text(markdown(board, &self.eval_filter));
                    }
                }
            });
        });
        if self.eval_filter != before {
            self.eval = None;
        }
    }

    /// The four numbers that frame everything else. A headline figure beats a
    /// chart when there is only one number to say — these are counts, not
    /// distributions, so they are stat tiles rather than a plot.
    fn headline(&self, ui: &mut egui::Ui, board: &Leaderboard) {
        ui.horizontal(|ui| {
            tile(
                ui,
                "contests",
                &board.contests.to_string(),
                &format!("{} with 2+ models answering", board.contested),
                theme::ACCENT,
            );
            let took = format!(
                "{:.0}%",
                if board.contests == 0 {
                    0.0
                } else {
                    100.0 * board.model_won as f64 / board.contests as f64
                }
            );
            tile(
                ui,
                "you took a model's text",
                &took,
                &format!(
                    "{} model · {} your own words · {} kept as written",
                    board.model_won, board.human_won, board.original_kept
                ),
                theme::GOOD,
            );
            let cost = board.total_cost();
            tile(
                ui,
                "spent on suggestions",
                &if cost > 0.0 {
                    format!("${cost:.2}")
                } else {
                    "—".into()
                },
                &format!("{} unpriced call(s)", board.unpriced_calls()),
                theme::WARN,
            );
            tile(
                ui,
                "tokens",
                &compact(board.total_tokens()),
                "input + output, per-unit review calls",
                theme::TEXT_DIM,
            );
        });
    }

    /// The table. Sorted by win rate, one row per model, bars sharing a single
    /// 0–100% scale so two rows can be compared by eye without reading either
    /// number.
    fn leaderboard(&self, ui: &mut egui::Ui, board: &Leaderboard) {
        theme::section_title(ui, "LEADERBOARD — YOUR PICK RATE, PER MODEL");
        ui.label(theme::dim(
            "A win is a decision whose final text came from that model, editing included. \
             Agreement is the weaker signal beside it: the same keep/rewrite/delete verdict \
             as yours, which says nothing about whether the words were any good.",
        ));
        ui.add_space(4.0);

        if board.standings.is_empty() {
            ui.label(theme::dim("no model has answered a decided unit yet"));
            return;
        }

        egui::Grid::new("eval_leaderboard")
            .num_columns(8)
            .spacing([14.0, 5.0])
            .striped(true)
            .show(ui, |ui| {
                for h in [
                    "model", "won", "agreed", "offered", "errors", "mean", "tokens", "cost",
                ] {
                    ui.label(theme::dim(h));
                }
                ui.end_row();

                for s in &board.standings {
                    let color = self.model_color(&s.model);
                    ui.horizontal(|ui| {
                        swatch(ui, color);
                        ui.label(RichText::new(&s.model).monospace().strong());
                    });

                    rate_cell(ui, s.win_pct(), s.wins, s.offered, color, edited_note(s));
                    rate_cell(
                        ui,
                        s.agreement_pct(),
                        s.agreed,
                        s.offered,
                        theme::TEXT_DIM,
                        String::new(),
                    );

                    ui.label(RichText::new(s.offered.to_string()).monospace());
                    let errors = if s.errors == 0 {
                        RichText::new("0").monospace().color(theme::TEXT_DIM)
                    } else {
                        RichText::new(format!("{} ({:.0}%)", s.errors, s.error_pct()))
                            .monospace()
                            .color(theme::BAD)
                    };
                    ui.label(errors);
                    ui.label(RichText::new(format!("{} ms", s.mean_latency_ms())).monospace());
                    ui.label(RichText::new(compact(s.tokens())).monospace())
                        .on_hover_text(format!(
                            "{} in · {} out · {} cached-in, over {} call(s)",
                            s.input_tokens, s.output_tokens, s.cache_read_tokens, s.calls
                        ));
                    cost_cell(ui, s);
                    ui.end_row();
                }
            });
    }

    /// Pairwise, because a leaderboard row hides who a model was up against.
    /// Two models with the same win rate are not equivalent if one of them was
    /// only ever in the room with the weakest model configuration.
    fn head_to_head(&self, ui: &mut egui::Ui, board: &Leaderboard) {
        if board.head_to_head.is_empty() {
            return;
        }
        theme::section_title(ui, "HEAD TO HEAD — WHEN BOTH WERE ON THE TABLE");
        ui.label(theme::dim(
            "Only the contests where both answered. The grey middle is every decision that \
             went to neither of them — your own words, the original text, or a third model.",
        ));
        ui.add_space(4.0);
        for h in &board.head_to_head {
            self.h2h_row(ui, h);
        }
    }

    fn h2h_row(&self, ui: &mut egui::Ui, h: &HeadToHead) {
        let a_color = self.model_color(&h.a);
        let b_color = self.model_color(&h.b);
        ui.horizontal(|ui| {
            ui.add_sized(
                [110.0, 16.0],
                egui::Label::new(RichText::new(&h.a).monospace().color(a_color))
                    .halign(egui::Align::RIGHT),
            );
            count(ui, h.a_wins, egui::Align::RIGHT);

            // One 100%-wide bar split three ways: a's wins, neither, b's wins.
            // A single stacked bar rather than two opposed ones, so the
            // undecided middle stays visible instead of being implied by a gap.
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(220.0, BAR_H + 2.0), egui::Sense::hover());
            let painter = ui.painter();
            let total = h.together.max(1) as f32;
            let mut x = rect.left();
            let segments = [
                (h.a_wins as f32 / total, a_color),
                ((h.together - h.decided()) as f32 / total, theme::GUTTER),
                (h.b_wins as f32 / total, b_color),
            ];
            for (frac, color) in segments {
                let w = frac * rect.width();
                if w <= 0.0 {
                    continue;
                }
                // A 2px gap between fills, so two adjacent segments of similar
                // lightness still read as two.
                let seg = egui::Rect::from_min_size(
                    egui::pos2(x, rect.top() + 1.0),
                    egui::vec2((w - 2.0).max(1.0), BAR_H),
                );
                painter.rect_filled(seg, 2.0, color);
                x += w;
            }
            resp.on_hover_text(format!(
                "{} won {}, {} won {}, neither {} — of {} contests both answered",
                h.a,
                h.a_wins,
                h.b,
                h.b_wins,
                h.together - h.decided(),
                h.together
            ));

            count(ui, h.b_wins, egui::Align::LEFT);
            ui.add_sized(
                [110.0, 16.0],
                egui::Label::new(RichText::new(&h.b).monospace().color(b_color))
                    .halign(egui::Align::LEFT),
            );
            ui.label(theme::dim(&format!("· {} together", h.together)));
        });
    }

    /// What each model reaches for, against what you reached for. This is the
    /// bias check the win rate cannot show: a model that answers "rewrite" to
    /// everything scores well on a branch full of bad comments and badly on a
    /// clean one, and its mix is what gives that away.
    fn verdict_mix(&self, ui: &mut egui::Ui, board: &Leaderboard) {
        if board.standings.iter().all(|s| s.verdicts.is_empty()) {
            return;
        }
        theme::section_title(ui, "VERDICT MIX — WHAT EACH MODEL REACHES FOR");
        ui.add_space(4.0);
        egui::Grid::new("eval_mix")
            .num_columns(3)
            .spacing([14.0, 5.0])
            .show(ui, |ui| {
                ui.label(theme::dim("you"));
                mix_bar(ui, &board.human_verdicts);
                ui.label(theme::dim(&mix_legend(&board.human_verdicts)));
                ui.end_row();
                for s in &board.standings {
                    if s.verdicts.is_empty() {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        swatch(ui, self.model_color(&s.model));
                        ui.label(RichText::new(&s.model).monospace());
                    });
                    mix_bar(ui, &s.verdicts);
                    ui.label(theme::dim(&mix_legend(&s.verdicts)));
                    ui.end_row();
                }
            });
    }

    /// Money, kept apart from the leaderboard on purpose: spend counts every
    /// call, including the ones on units you skipped, so its denominator is
    /// not the one the win rate uses.
    fn spend(&self, ui: &mut egui::Ui, board: &Leaderboard) {
        theme::section_title(ui, "SPEND — EVERY CALL, DECIDED OR NOT");
        ui.label(theme::dim(
                "Per-unit review calls only. Whole-branch reviews and follow-up fix sessions run the same \
             CLIs and are not counted here. Cost is the CLI's own figure where it reports one, \
             otherwise priced from the model configuration's $/Mtok rates in settings.",
        ));
        if board.excluded_unblinded > 0 {
            // Spend cannot honour the blinded filter: a call on a unit that was
            // never decided has no blinding to be filtered by. Saying so here,
            // beside the column it distorts, beats burying it below.
            ui.label(theme::dim(
                "Spend is the one thing the blinded filter does not touch — a call costs what \
                 it cost whether or not its decision is being scored — so per-win reads high \
                 while the filter is hiding wins.",
            ));
        }
        ui.add_space(4.0);
        egui::Grid::new("eval_spend")
            .num_columns(6)
            .spacing([14.0, 5.0])
            .striped(true)
            .show(ui, |ui| {
                for h in ["model", "calls", "in", "out", "cached in", "per win"] {
                    ui.label(theme::dim(h));
                }
                ui.end_row();
                for s in &board.standings {
                    ui.horizontal(|ui| {
                        swatch(ui, self.model_color(&s.model));
                        ui.label(RichText::new(&s.model).monospace());
                    });
                    ui.label(RichText::new(s.calls.to_string()).monospace());
                    ui.label(RichText::new(compact(s.input_tokens)).monospace());
                    ui.label(RichText::new(compact(s.output_tokens)).monospace());
                    ui.label(RichText::new(compact(s.cache_read_tokens)).monospace());
                    match s.cost_per_win() {
                        Some(usd) => {
                            ui.label(RichText::new(format!("${usd:.3}")).monospace())
                                .on_hover_text(format!(
                                    "${:.4} total ÷ {} suggestion(s) you took",
                                    s.cost_usd, s.wins
                                ));
                        }
                        None => {
                            ui.label(theme::dim("—")).on_hover_text(if s.priced_calls == 0 {
                                "no cost recorded for this model configuration — its CLI reports none and no \
                                 rates are set in settings"
                            } else {
                                "nothing won yet"
                            });
                        }
                    }
                    ui.end_row();
                }
            });
    }

    /// The model configuration colour for a model *by name*, so sorting or filtering the table
    /// never repaints a model into another's colour. A model with no model configuration left
    /// in settings — renamed, or removed after it was used — keeps its history
    /// and gets the neutral ink rather than borrowing someone else's hue.
    fn model_color(&self, model: &str) -> egui::Color32 {
        match self.settings.models.iter().position(|m| m.name == model) {
            Some(i) => theme::model_color(i),
            None => theme::TEXT_DIM,
        }
    }

    /// Rebuild the aggregate. Deliberately explicit rather than recomputed per
    /// frame: it is two full table scans, and the page repaints on every
    /// pointer move.
    pub fn reload_eval(&mut self) {
        self.eval_repos = self.db.repos_with_history();
        // A filter pinned to a repository with no history left would silently
        // show an empty page; fall back to everything.
        if self
            .eval_filter
            .repo
            .as_ref()
            .is_some_and(|r| !self.eval_repos.contains(r))
        {
            self.eval_filter.repo = None;
        }
        self.eval = Some(Leaderboard::from_db(&self.db, &self.eval_filter));
    }

    pub fn open_eval(&mut self) {
        if self.screen != Screen::Eval {
            self.prev_screen = self.screen;
            self.goto(Screen::Eval);
            self.eval = None;
        }
    }
}

/// A headline number with its label above and its breakdown below.
fn tile(ui: &mut egui::Ui, label: &str, value: &str, sub: &str, color: egui::Color32) {
    egui::Frame::group(ui.style())
        .fill(theme::PANEL)
        .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(50)))
        .inner_margin(egui::Margin::same(7.0))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_min_width(168.0);
                ui.label(theme::dim(label));
                ui.label(RichText::new(value).heading().strong().color(color));
                ui.label(theme::dim(sub));
            });
        });
}

/// A rate as bar + `n/d`. The denominator is not optional: this table's rates
/// are routinely built on a handful of decisions, and a bare percentage
/// invites reading a 2-of-3 as a result.
fn rate_cell(ui: &mut egui::Ui, pct: f64, n: usize, d: usize, color: egui::Color32, note: String) {
    ui.horizontal(|ui| {
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(BAR_W, BAR_H + 2.0), egui::Sense::hover());
        let painter = ui.painter();
        let track = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + 1.0),
            egui::vec2(rect.width(), BAR_H),
        );
        painter.rect_filled(track, 2.0, theme::RAISED);
        let w = (pct / 100.0).clamp(0.0, 1.0) as f32 * rect.width();
        if w > 0.0 {
            painter.rect_filled(
                egui::Rect::from_min_size(track.min, egui::vec2(w.max(2.0), BAR_H)),
                2.0,
                color,
            );
        }
        if !note.is_empty() {
            resp.on_hover_text(note);
        }
        // Text in ink, never in the series colour — the swatch beside the
        // model name already carries identity.
        ui.label(RichText::new(format!("{pct:.0}%")).monospace().strong());
        ui.label(theme::dim(&format!("{n}/{d}")));
    });
}

/// A win count in a fixed-width cell. Letting it size to its digits would
/// shift the bar beside it, and three head-to-head rows whose bars start at
/// three different offsets cannot be compared by eye — which is the only
/// reason the bars are there.
fn count(ui: &mut egui::Ui, n: usize, align: egui::Align) {
    ui.add_sized(
        [24.0, 16.0],
        egui::Label::new(RichText::new(n.to_string()).monospace().strong()).halign(align),
    );
}

fn edited_note(s: &Standing) -> String {
    if s.wins_edited == 0 {
        String::new()
    } else {
        format!(
            "{} of {} wins were reworded before saving — the model got you started, not finished",
            s.wins_edited, s.wins
        )
    }
}

fn cost_cell(ui: &mut egui::Ui, s: &Standing) {
    match s.cost_per_call() {
        Some(per_call) => {
            let text = if s.cost_usd >= 0.01 {
                format!("${:.2}", s.cost_usd)
            } else {
                format!("${:.4}", s.cost_usd)
            };
            let label = ui.label(RichText::new(text).monospace());
            let source = if s.estimated_calls > 0 {
                format!(
                    "{} call(s) priced from the model configuration's rates, not by the CLI",
                    s.estimated_calls
                )
            } else {
                "as the CLI priced it".to_string()
            };
            label.on_hover_text(format!("${per_call:.4} per call · {source}"));
        }
        None => {
            ui.label(theme::dim("—")).on_hover_text(
                "unmeasured, not free: this CLI reports no cost and the model configuration has no \
                 $/Mtok rates set in settings",
            );
        }
    }
}

/// The verdict palette is the right one here — these *are* the verdicts, the
/// same four colours the review screen paints them in — so the mix bar reuses
/// it rather than introducing a second set of hues for the same four things.
fn verdict_color(action: &str) -> egui::Color32 {
    match action {
        "keep" => theme::GOOD,
        "rewrite" => theme::WARN,
        "delete" => theme::BAD,
        "flag" => theme::ACCENT,
        _ => theme::TEXT_DIM,
    }
}

fn mix_bar(ui: &mut egui::Ui, counts: &std::collections::BTreeMap<String, usize>) {
    let total: usize = counts.values().sum();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(220.0, BAR_H + 2.0), egui::Sense::hover());
    if total == 0 {
        return;
    }
    let painter = ui.painter();
    let mut x = rect.left();
    for (action, n) in counts {
        let w = *n as f32 / total as f32 * rect.width();
        if w <= 0.0 {
            continue;
        }
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(x, rect.top() + 1.0),
                egui::vec2((w - 2.0).max(1.0), BAR_H),
            ),
            2.0,
            verdict_color(action),
        );
        x += w;
    }
    resp.on_hover_text(mix_legend(counts));
}

/// The bar is never colour-alone: the counts are spelled out beside it.
fn mix_legend(counts: &std::collections::BTreeMap<String, usize>) -> String {
    let total: usize = counts.values().sum();
    if total == 0 {
        return "no answers".into();
    }
    counts
        .iter()
        .map(|(a, n)| format!("{a} {:.0}%", 100.0 * *n as f64 / total as f64))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn caveats(ui: &mut egui::Ui, board: &Leaderboard) {
    theme::section_title(ui, "HOW MUCH OF THIS TO BELIEVE");
    for line in board.caveats() {
        ui.horizontal_top(|ui| {
            ui.label(RichText::new("!").small().strong().color(theme::WARN));
            ui.label(theme::dim(&line));
        });
    }
    if board.excluded_unblinded > 0 {
        ui.label(theme::dim(&format!(
            "· {} answer(s) hidden by the blinded-only filter",
            board.excluded_unblinded
        )));
    }
    ui.add_space(2.0);
    ui.label(theme::dim(
        "None of this is accuracy. There is no ground truth for whether a comment earns its \
         place — your judgement is the label, so every rate above says \"agrees with you\", \
         bounded by how often you agree with yourself.",
    ));
}

fn swatch(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, color);
}

/// Thousands as `12.4k`: the table has eight columns and a raw token count is
/// the widest thing that would go in any of them.
fn compact(n: i64) -> String {
    match n {
        0 => "—".into(),
        n if n < 10_000 => n.to_string(),
        n if n < 1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        n => format!("{:.2}M", n as f64 / 1_000_000.0),
    }
}

/// The last two path segments — enough to tell two checkouts apart without
/// spending a combo box on a full Windows path.
fn short_repo(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    match parts.len() {
        0 => path.to_string(),
        1 => parts[0].to_string(),
        n => format!("{}/{}", parts[n - 2], parts[n - 1]),
    }
}

/// The whole page as text, for pasting somewhere the app is not. Carries the
/// filter and the caveats with it: a leaderboard that arrives without them
/// reads as a benchmark, which it is not.
fn markdown(board: &Leaderboard, filter: &Filter) -> String {
    let mut out = String::from("# Model evaluation\n\n");
    out.push_str(&format!(
        "Scope: {} · {}\n\n",
        filter.repo.as_deref().unwrap_or("all repositories"),
        if filter.blinded_only {
            "blinded decisions only"
        } else {
            "all decisions"
        }
    ));
    out.push_str(&format!(
        "{} contests ({} with 2+ models). You took a model's text {} time(s), wrote your own \
         {} time(s), kept the original {} time(s).\n\n",
        board.contests, board.contested, board.model_won, board.human_won, board.original_kept
    ));
    out.push_str("| model | won | agreed | offered | errors | mean ms | tokens | cost |\n");
    out.push_str("|---|---|---|---|---|---|---|---|\n");
    for s in &board.standings {
        let cost = match s.cost_per_call() {
            Some(_) => format!("${:.4}", s.cost_usd),
            None => "—".into(),
        };
        out.push_str(&format!(
            "| {} | {:.0}% ({}/{}) | {:.0}% ({}/{}) | {} | {} | {} | {} | {} |\n",
            s.model,
            s.win_pct(),
            s.wins,
            s.offered,
            s.agreement_pct(),
            s.agreed,
            s.offered,
            s.offered,
            s.errors,
            s.mean_latency_ms(),
            s.tokens(),
            cost,
        ));
    }
    if !board.head_to_head.is_empty() {
        out.push_str("\n## Head to head\n\n");
        for h in &board.head_to_head {
            out.push_str(&format!(
                "- {} {} — {} {} (of {} contests both answered)\n",
                h.a, h.a_wins, h.b_wins, h.b, h.together
            ));
        }
    }
    out.push_str("\n## How much of this to believe\n\n");
    for line in board.caveats() {
        out.push_str(&format!("- {line}\n"));
    }
    out.push_str(
        "\nThese are agreement rates with one reviewer, not accuracy: there is no ground truth \
         for whether a comment earns its place.\n",
    );
    out
}
