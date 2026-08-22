use egui::{Key, Modifiers, RichText};

use crate::app::CraApp;
use crate::ui::theme;

impl CraApp {
    pub fn ui_repo_picker(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let typing = ctx.wants_keyboard_input();
        let n = self.settings.recent_repos.len();
        if !typing && n > 0 {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                self.repo_sel = (self.repo_sel + 1).min(n - 1);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                self.repo_sel = self.repo_sel.saturating_sub(1);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                if let Some(p) = self.settings.recent_repos.get(self.repo_sel).cloned() {
                    self.select_repo(p);
                    return;
                }
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::X)) && self.repo_sel < n {
                let removed = self.settings.recent_repos.remove(self.repo_sel);
                self.settings.save(&self.db);
                self.note("repo", &format!("forgot {removed}"));
                self.repo_sel = self.repo_sel.min(self.settings.recent_repos.len().saturating_sub(1));
            }
        }

        ui.heading("Pick repository");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(theme::dim("path:"));
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.repo_input)
                    .hint_text("/absolute/path/to/repo")
                    .desired_width(420.0)
                    .font(egui::TextStyle::Monospace),
            );
            let submitted =
                resp.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter));
            if ui.button("Add + open").clicked() || submitted {
                let p = self.repo_input.clone();
                if !p.trim().is_empty() {
                    self.select_repo(p);
                }
            }
        });
        if let Some(err) = &self.repo_error {
            ui.colored_label(theme::BAD, err);
        }

        ui.add_space(8.0);
        theme::section_title(ui, "RECENT REPOSITORIES");
        let mut open: Option<String> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, path) in self.settings.recent_repos.clone().iter().enumerate() {
                let selected = i == self.repo_sel;
                let name = crate::gitio::repo_name(path);
                let label = format!("{name}  —  {path}");
                let resp = ui.selectable_label(
                    selected,
                    RichText::new(label).monospace(),
                );
                if resp.clicked() {
                    self.repo_sel = i;
                }
                if resp.double_clicked() {
                    open = Some(path.clone());
                }
            }
            if self.settings.recent_repos.is_empty() {
                ui.label(theme::dim("none yet — enter a path above"));
            }
        });
        if let Some(p) = open {
            self.select_repo(p);
        }
    }
}
