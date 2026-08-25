use egui::{Key, Modifiers, RichText};

use crate::app::{CraApp, RefTab};
use crate::gitio::PrInfo;
use crate::ui::theme;

enum RefAction {
    Branch(String),
    Pr(PrInfo),
    WorkingTree,
}

impl CraApp {
    pub fn ui_ref_picker(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let typing = ctx.wants_keyboard_input();
        let mut action: Option<RefAction> = None;

        let list_len = match self.ref_tab {
            RefTab::Branches => self.branches.len(),
            RefTab::Prs => self.prs.len(),
        };
        if !typing {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
                self.ref_tab = match self.ref_tab {
                    RefTab::Branches => RefTab::Prs,
                    RefTab::Prs => RefTab::Branches,
                };
                self.ref_sel = 0;
            }
            if list_len > 0 {
                if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                    self.ref_sel = (self.ref_sel + 1).min(list_len - 1);
                }
                if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                    self.ref_sel = self.ref_sel.saturating_sub(1);
                }
                if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                    action = match self.ref_tab {
                        RefTab::Branches => self
                            .branches
                            .get(self.ref_sel)
                            .map(|b| RefAction::Branch(b.name.clone())),
                        RefTab::Prs => self.prs.get(self.ref_sel).map(|p| RefAction::Pr(p.clone())),
                    };
                }
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::W)) {
                action = Some(RefAction::WorkingTree);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::R)) {
                self.load_refs();
            }
        }

        ui.heading("Pick branch / PR");
        ui.horizontal(|ui| {
            if ui
                .selectable_label(self.ref_tab == RefTab::Branches, "Local branches [Tab]")
                .clicked()
            {
                self.ref_tab = RefTab::Branches;
                self.ref_sel = 0;
            }
            if ui
                .selectable_label(self.ref_tab == RefTab::Prs, "Open PRs [Tab]")
                .clicked()
            {
                self.ref_tab = RefTab::Prs;
                self.ref_sel = 0;
            }
            ui.separator();
            if ui.button("Review working tree [W]").clicked() {
                action = Some(RefAction::WorkingTree);
            }
            if ui.button("Refresh [R]").clicked() {
                self.load_refs();
            }
        });
        if let Some(err) = self.ref_error.clone() {
            ui.colored_label(theme::BAD, err);
        }
        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| match self.ref_tab {
                RefTab::Branches => {
                    let default = self
                        .repo
                        .as_ref()
                        .map(|r| r.default_branch.clone())
                        .unwrap_or_default();
                    for (i, b) in self.branches.clone().iter().enumerate() {
                        let is_base = b.name == default;
                        let mut text = format!(
                            "{:<40} {} {:>18}  {}",
                            b.name,
                            b.sha,
                            b.age,
                            crate::app::truncate(&b.subject, 60)
                        );
                        if is_base {
                            text.push_str("  [base]");
                        }
                        let resp =
                            ui.selectable_label(i == self.ref_sel, RichText::new(text).monospace());
                        if resp.clicked() {
                            self.ref_sel = i;
                        }
                        if resp.double_clicked() {
                            action = Some(RefAction::Branch(b.name.clone()));
                        }
                    }
                    if self.branches.is_empty() {
                        ui.label(theme::dim("no local branches found"));
                    }
                }
                RefTab::Prs => {
                    if self.prs_loading {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(theme::dim("loading open PRs via gh…"));
                        });
                    }
                    if let Some(e) = &self.prs_error {
                        ui.colored_label(theme::BAD, format!("gh: {e}"));
                    }
                    for (i, pr) in self.prs.clone().iter().enumerate() {
                        let author = pr
                            .author
                            .as_ref()
                            .map(|a| a.login.clone())
                            .unwrap_or_default();
                        let text = format!(
                            "#{:<5} {:<60} {} → {}  @{}",
                            pr.number,
                            crate::app::truncate(&pr.title, 58),
                            pr.head_ref,
                            pr.base_ref,
                            author
                        );
                        let resp =
                            ui.selectable_label(i == self.ref_sel, RichText::new(text).monospace());
                        if resp.clicked() {
                            self.ref_sel = i;
                        }
                        if resp.double_clicked() {
                            action = Some(RefAction::Pr(pr.clone()));
                        }
                    }
                    if self.prs.is_empty() && !self.prs_loading && self.prs_error.is_none() {
                        ui.label(theme::dim("no open PRs"));
                    }
                }
            });

        match action {
            Some(RefAction::Branch(name)) => self.select_branch(&name),
            Some(RefAction::Pr(pr)) => self.select_pr(&pr),
            Some(RefAction::WorkingTree) => self.select_working_tree(),
            None => {}
        }
    }
}
