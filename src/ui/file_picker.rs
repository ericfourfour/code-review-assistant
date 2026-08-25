use egui::{Key, Modifiers, RichText};

use crate::app::CraApp;
use crate::ui::theme;

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
        ui.horizontal(|ui| {
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
        ui.add_space(4.0);

        if let Some(plan) = &self.plan {
            let rows: Vec<(String, usize, usize, u32, usize)> = plan
                .files
                .iter()
                .map(|f| {
                    let code = f.units.iter().filter(|u| u.is_code()).count();
                    let risk = f
                        .units
                        .iter()
                        .map(|u| crate::triage::assess(u).score)
                        .max()
                        .unwrap_or(0);
                    (f.path.clone(), f.units.len() - code, code, risk, f.decided)
                })
                .collect();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for (i, (path, comments, code, risk, decided)) in rows.iter().enumerate() {
                    let text = format!(
                        "{path:<70} {comments:>3} comments {code:>3} code  risk {risk:>3}  ({decided} decided)"
                    );
                    let resp =
                        ui.selectable_label(i == self.file_sel, RichText::new(text).monospace());
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

        if let Some(idx) = start_at {
            self.start_review(ctx, idx);
        }
    }
}
