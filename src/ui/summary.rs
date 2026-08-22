use egui::{Key, Modifiers, RichText};

use crate::app::{CraApp, Screen};
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
        }

        ui.heading("Review complete");
        ui.add_space(6.0);
        if let Some(p) = &self.plan {
            let (decided, committed) = self.db.decision_counts(p.session_id);
            egui::Grid::new("summary_grid").num_columns(2).spacing([12.0, 3.0]).show(ui, |ui| {
                ui.label(theme::dim("ref"));
                ui.label(RichText::new(format!("{} [{}]", p.ref_name, p.ref_kind.label())).monospace());
                ui.end_row();
                ui.label(theme::dim("base"));
                ui.label(RichText::new(&p.base_ref).monospace());
                ui.end_row();
                ui.label(theme::dim("comments reviewed"));
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
    }
}
