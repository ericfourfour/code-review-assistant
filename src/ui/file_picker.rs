use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, Key, Modifiers, RichText, TextStyle};

use crate::app::CraApp;
use crate::ui::theme;

/// How much of the row the path gets. Longer paths take the space they need
/// and push their own row's columns right; padding every row to the longest
/// path in a deep tree would leave most of the list as whitespace.
const PATH_WIDTH: usize = 62;

impl CraApp {
    pub fn ui_file_picker(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let typing = ctx.wants_keyboard_input();
        let n = self.plan.as_ref().map(|p| p.files.len()).unwrap_or(0);
        let mut start_at: Option<usize> = None;

        if !typing && n > 0 {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                self.file_sel = (self.file_sel + 1).min(n - 1);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                self.file_sel = self.file_sel.saturating_sub(1);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                start_at = Some(self.file_sel);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::S)) {
                start_at = Some(0);
            }
        }

        ui.heading("Pick file — or start the full review");
        let mut back_to_review = false;
        ui.horizontal(|ui| {
            // Picking a file restarts the review at that file's first unit,
            // which is a different thing from going back to where you were.
            // Both are offered because both are wanted, and only one of them
            // costs another round of model calls.
            if let Some(at) = self.review_in_progress() {
                let paused = self
                    .candidates
                    .iter()
                    .filter(|c| matches!(c, crate::app::CandidateState::Paused(_)))
                    .count();
                let label = match paused {
                    0 => format!("◀ Back to {at}"),
                    n => format!("◀ Back to {at} ({n} paused)"),
                };
                if ui
                    .button(label)
                    .on_hover_text("returns to the unit you left — nothing is re-asked")
                    .clicked()
                {
                    back_to_review = true;
                }
                ui.separator();
            }
            if ui.button("▶ Start full review [S]").clicked() {
                start_at = Some(0);
            }
            if let Some(p) = &self.plan {
                ui.label(theme::dim(&format!(
                    "{} reviewable units across {} files on {}",
                    p.total_units(),
                    p.files.len(),
                    p.ref_name
                )));
            }
        });
        // The same three numbers the rows carry, summed, so the size of the
        // job is on screen before any file is picked.
        if let Some(p) = &self.plan {
            let added: usize = p.files.iter().map(|f| f.line_changes.0).sum();
            let removed: usize = p.files.iter().map(|f| f.line_changes.1).sum();
            let to_review: usize = p.files.iter().map(|f| f.review_lines()).sum();
            let total: usize = p.files.iter().map(|f| f.total_lines).sum();
            ui.horizontal(|ui| {
                ui.label(theme::dim("diff"));
                ui.label(RichText::new(format!("+{added}")).small().color(theme::ADDED));
                ui.label(RichText::new(format!("−{removed}")).small().color(theme::REMOVED));
                ui.label(theme::dim(&format!(
                    " ·  {to_review} line(s) to review{}",
                    match total {
                        0 => String::new(),
                        t => format!(" of {t} in these files"),
                    }
                )));
            });
        }
        // Say what is missing and why, or a plan shortened by past decisions
        // looks like an extractor that lost things.
        if let Some(n) = self.plan.as_ref().map(|p| p.skipped_decided).filter(|n| *n > 0) {
            ui.label(theme::dim(&format!(
                "{n} more unit(s) already decided in this repository and left out"
            )));
        }
        ui.add_space(4.0);

        if let Some(plan) = &self.plan {
            let rows: Vec<Row> = plan
                .files
                .iter()
                .map(|f| {
                    let code = f.units.iter().filter(|u| u.is_code()).count();
                    Row {
                        path: f.path.clone(),
                        comments: f.units.len() - code,
                        code,
                        risk: f.units.iter().map(|u| crate::triage::assess(u).score).max().unwrap_or(0),
                        decided: f.decided,
                        line_changes: f.line_changes,
                        review_lines: f.review_lines(),
                        total_lines: f.total_lines,
                    }
                })
                .collect();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for (i, row) in rows.iter().enumerate() {
                    let resp = ui.selectable_label(i == self.file_sel, row.job(ui));
                    if resp.clicked() {
                        self.file_sel = i;
                    }
                    if resp.double_clicked() {
                        start_at = Some(i);
                    }
                }
            });
        } else {
            ui.label(theme::dim("no plan — pick a branch or PR first"));
        }

        if back_to_review {
            self.goto(crate::app::Screen::Review);
        }
        if let Some(idx) = start_at {
            self.start_review(ctx, idx);
        }
    }
}

/// One file's line in the picker, gathered off the plan before the list is
/// drawn so the borrow of the plan ends before the click handlers need `self`.
struct Row {
    path: String,
    comments: usize,
    code: usize,
    risk: u32,
    decided: usize,
    line_changes: (usize, usize),
    review_lines: usize,
    total_lines: usize,
}

impl Row {
    /// The row as one monospace line. A layout job rather than a formatted
    /// string because the `+`/`-` counts have to carry the diff colours — the
    /// numbers are read at a glance and the sign alone is easy to miss.
    fn job(&self, ui: &egui::Ui) -> LayoutJob {
        let font = TextStyle::Monospace.resolve(ui.style());
        let mut job = LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        let mut push = |text: String, color: Color32| {
            job.append(&text, 0.0, TextFormat { font_id: font.clone(), color, ..Default::default() })
        };

        let text = ui.visuals().text_color();
        let (added, removed) = self.line_changes;
        push(
            format!(
                "{:<PATH_WIDTH$} {:>3} comments {:>3} code  ",
                self.path, self.comments, self.code
            ),
            text,
        );
        push(format!("{:>6}", format!("+{added}")), theme::ADDED);
        push(format!("{:>6}", format!("-{removed}")), theme::REMOVED);
        // An unreadable file is a question mark rather than a zero: "0 lines"
        // is a claim about the file, and this does not know that. Both marks
        // are ASCII so the columns stay aligned whatever the mono font has.
        let of = match self.total_lines {
            0 => "?".to_string(),
            t => t.to_string(),
        };
        push(
            format!(
                "  {:>11} lines  risk {:>3}  ({} decided)",
                format!("{}/{of}", self.review_lines),
                self.risk,
                self.decided
            ),
            text,
        );
        job
    }
}
