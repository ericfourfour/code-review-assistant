use egui::{Key, Modifiers, RichText};

use crate::app::{CraApp, RefTab};
use crate::gitio::PrInfo;
use crate::ui::theme;

enum RefAction {
    Branch(String),
    Pr(PrInfo),
    WorkingTree,
    Staged,
    Recheck,
    Followup,
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
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::S)) {
                action = Some(RefAction::Staged);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::U)) {
                self.settings.include_untracked = !self.settings.include_untracked;
                self.settings.save(&self.db);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::C)) {
                action = Some(RefAction::Recheck);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::N)) {
                action = Some(RefAction::Followup);
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
            if ui
                .button("Review working tree [W]")
                .on_hover_text("everything uncommitted, staged or not, against HEAD")
                .clicked()
            {
                action = Some(RefAction::WorkingTree);
            }
            if ui
                .checkbox(&mut self.settings.include_untracked, "untracked [U]")
                .on_hover_text(
                    "working-tree reviews also take in files git does not track yet \
                     (shown as new-file diffs; .gitignore still applies)",
                )
                .changed()
            {
                self.settings.save(&self.db);
            }
            if ui
                .button("Review staged [S]")
                .on_hover_text("only what `git add` staged — the commit being prepared")
                .clicked()
            {
                action = Some(RefAction::Staged);
            }
            if ui
                .button("Re-check past decisions [C]")
                .on_hover_text(
                    "Re-judge comments you have already decided. Comparing your verdicts to \
your earlier ones measures how consistent you are, which is the ceiling any model can be \
scored against. Nothing is written to disk.",
                )
                .clicked()
            {
                action = Some(RefAction::Recheck);
            }
            let open_notes = self
                .repo
                .as_ref()
                .map(|r| self.db.count_open_notes(&r.path))
                .unwrap_or(0);
            if ui
                .button(format!("Follow-up notes ({open_notes}) [N]"))
                .on_hover_text(
                    "the backlog survives the session that wrote it — triage past notes and \
                     start a fix session without re-running a review",
                )
                .clicked()
            {
                action = Some(RefAction::Followup);
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
            Some(RefAction::Staged) => self.select_staged(),
            Some(RefAction::Recheck) => self.start_recheck(ctx, 25),
            Some(RefAction::Followup) => self.open_followup(),
            None => {}
        }
    }
}
