use egui::{Key, Modifiers, RichText};

use crate::app::CraApp;
use crate::settings::ModelSlot;
use crate::ui::theme;

impl CraApp {
    pub fn ui_settings(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::S)) {
            self.close_settings();
            return;
        }

        ui.heading("Settings");
        ui.label(theme::dim(
            "stored in the local sqlite database; saved on close",
        ));
        ui.add_space(6.0);

        theme::section_title(ui, "REVIEWER MODELS");
        ui.label(theme::dim(
            "command templates are tokenized on whitespace; {prompt} is replaced with the prompt, \
             otherwise the prompt is piped to stdin. CLIs must be installed and authenticated.",
        ));
        let mut remove: Option<usize> = None;
        egui::Grid::new("models_grid")
            .num_columns(6)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                ui.label(theme::dim("on"));
                ui.label(theme::dim("name"));
                ui.label(theme::dim("command template"));
                ui.label(theme::dim("co-author (Name <email>)"));
                ui.label(theme::dim(""));
                ui.label(theme::dim("hotkey"));
                ui.end_row();
                for (i, m) in self.settings.models.iter_mut().enumerate() {
                    ui.checkbox(&mut m.enabled, "");
                    ui.add(
                        egui::TextEdit::singleline(&mut m.name)
                            .desired_width(90.0)
                            .font(egui::TextStyle::Monospace),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut m.command)
                            .desired_width(320.0)
                            .font(egui::TextStyle::Monospace),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut m.coauthor)
                            .desired_width(280.0)
                            .font(egui::TextStyle::Monospace),
                    );
                    if ui.small_button("✕").clicked() {
                        remove = Some(i);
                    }
                    ui.label(
                        RichText::new(format!("[{}]", i + 1))
                            .monospace()
                            .color(theme::model_color(i)),
                    );
                    ui.end_row();
                }
            });
        if let Some(i) = remove {
            self.settings.models.remove(i);
        }
        if ui.button("+ add model").clicked() {
            self.settings.models.push(ModelSlot {
                name: "model".into(),
                command: "mycli -p {prompt}".into(),
                coauthor: "Model <model@example.com>".into(),
                enabled: true,
            });
        }

        ui.add_space(8.0);
        theme::section_title(ui, "GIT / GITHUB");
        egui::Grid::new("git_grid")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                ui.label(theme::dim("fallback base branch"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.default_base)
                        .desired_width(160.0)
                        .font(egui::TextStyle::Monospace),
                );
                ui.end_row();
                ui.label(theme::dim("gh CLI path"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.gh_path)
                        .desired_width(160.0)
                        .font(egui::TextStyle::Monospace),
                );
                ui.end_row();
            });

        ui.add_space(8.0);
        theme::section_title(ui, "REVIEW");
        egui::Grid::new("review_grid")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                ui.label(theme::dim("model timeout (s)"));
                ui.add(egui::DragValue::new(&mut self.settings.model_timeout_secs).range(5..=900));
                ui.end_row();
                ui.label(theme::dim("context lines"));
                ui.add(egui::DragValue::new(&mut self.settings.context_lines).range(2..=60));
                ui.end_row();
            });

        ui.add_space(10.0);
        if ui
            .button(RichText::new("Save and close  [Ctrl+S / Esc]").strong())
            .clicked()
        {
            self.close_settings();
        }
    }
}
