use egui::{Key, Modifiers, RichText};

use crate::app::CraApp;
use crate::discover::{self, DiscoveredRepo};
use crate::ui::theme;

/// One selectable line in the picker: a remembered path, or a discovered
/// repository — a clone found under the home folder, a repository from the
/// GitHub listing, or both merged into one.
enum Row {
    Recent(String),
    Discovered(DiscoveredRepo),
}

impl CraApp {
    pub fn ui_repo_picker(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        // Streaming discovery starts itself the first time the picker is
        // shown with a stale cache; a fresh cache costs nothing here.
        self.refresh_repos(false);

        // The visible rows are decided before anything is drawn, so the
        // keyboard, the mouse and the paint all agree on indices.
        let now = chrono::Utc::now().timestamp();
        let cutoff_secs = self.settings.repo_max_age_days as i64 * 86_400;
        let recents: Vec<String> = self.settings.recent_repos.clone();
        let mut rows: Vec<Row> = recents.iter().cloned().map(Row::Recent).collect();
        let n_recent = rows.len();
        let mut hidden_old = 0usize;
        let mut hidden_excluded = 0usize;
        for r in &self.discovered {
            if discover::is_excluded(&self.settings.excluded_repos, r) {
                hidden_excluded += 1;
                continue;
            }
            // Already listed above under RECENT — one row per repository.
            if r.path.as_ref().is_some_and(|p| recents.contains(p)) {
                continue;
            }
            // A repo whose age is unknown (0) is shown: hiding is for
            // confirmed staleness, not missing data.
            if cutoff_secs > 0 && r.last_update > 0 && now - r.last_update > cutoff_secs {
                hidden_old += 1;
                continue;
            }
            rows.push(Row::Discovered(r.clone()));
        }
        let n = rows.len();
        self.repo_sel = self.repo_sel.min(n.saturating_sub(1));

        let typing = ctx.wants_keyboard_input();
        let mut open: Option<usize> = None;
        let mut drop_row: Option<usize> = None;
        if !typing {
            if n > 0 {
                if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                    self.repo_sel = (self.repo_sel + 1).min(n - 1);
                }
                if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                    self.repo_sel = self.repo_sel.saturating_sub(1);
                }
                if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                    open = Some(self.repo_sel);
                }
                if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::X)) {
                    drop_row = Some(self.repo_sel);
                }
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::R)) {
                self.refresh_repos(true);
            }
        }

        // Labels ahead of the paint loop, so the loop can hand `self` to the
        // row helper without still borrowing the rows.
        let labels: Vec<(String, &'static str)> = rows
            .iter()
            .map(|row| match row {
                Row::Recent(path) => (
                    format!("{}  —  {}", crate::gitio::repo_name(path), path),
                    "forget (X)",
                ),
                Row::Discovered(r) => {
                    let age = discover::age_label(now, r.last_update);
                    let place = match (&r.path, &r.slug) {
                        (Some(p), _) => p.clone(),
                        (None, Some(s)) => format!("gh:{s}  (cloned when opened)"),
                        (None, None) => String::new(),
                    };
                    (
                        format!("{:<30} {:>5}  {}", r.name, age, place),
                        "exclude (X)",
                    )
                }
            })
            .collect();

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
            let submitted = resp.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter));
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
        if let Some(slug) = self.cloning.clone() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(theme::dim(&format!("cloning {slug}…")));
            });
        }

        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if n_recent > 0 {
                    theme::section_title(ui, "RECENT REPOSITORIES");
                    for (i, label) in labels.iter().enumerate().take(n_recent) {
                        self.picker_row(ui, i, label, &mut open, &mut drop_row);
                    }
                    ui.add_space(8.0);
                }
                self.discovered_header(ui, hidden_old, hidden_excluded);
                for (i, label) in labels.iter().enumerate().take(n).skip(n_recent) {
                    self.picker_row(ui, i, label, &mut open, &mut drop_row);
                }
                if n == n_recent && !self.scanning_local && !self.scanning_gh {
                    ui.label(theme::dim("nothing discovered — enter a path above"));
                }
            });

        if let Some(i) = drop_row {
            match rows.get(i) {
                Some(Row::Recent(path)) => {
                    self.settings.recent_repos.retain(|r| r != path);
                    self.settings.save(&self.db);
                    self.note("repo", &format!("forgot {path}"));
                }
                Some(Row::Discovered(r)) => {
                    // Excluded by path for a clone, by slug for a remote-only
                    // row — the same keys the settings screen edits.
                    let key = r.key().to_string();
                    if !key.is_empty() {
                        self.settings.excluded_repos.push(key.clone());
                        self.settings.save(&self.db);
                        self.note("repo", &format!("excluded {key} (undo in settings)"));
                    }
                }
                None => {}
            }
            self.repo_sel = self.repo_sel.min(rows.len().saturating_sub(2));
            return;
        }
        if let Some(i) = open {
            match rows.into_iter().nth(i) {
                Some(Row::Recent(path)) => self.select_repo(path),
                Some(Row::Discovered(r)) => match (r.path, r.slug) {
                    (Some(p), _) => self.select_repo(p),
                    (None, Some(slug)) => self.clone_and_open(slug),
                    (None, None) => {}
                },
                None => {}
            }
        }
    }

    fn picker_row(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        (label, drop_hint): &(String, &'static str),
        open: &mut Option<usize>,
        drop_row: &mut Option<usize>,
    ) {
        ui.horizontal(|ui| {
            if ui.small_button("✕").on_hover_text(*drop_hint).clicked() {
                *drop_row = Some(i);
            }
            let resp = ui.selectable_label(i == self.repo_sel, RichText::new(label).monospace());
            if resp.clicked() {
                self.repo_sel = i;
            }
            if resp.double_clicked() {
                *open = Some(i);
            }
        });
    }

    /// Section title plus the live state of the scan: spinner while either
    /// source is still streaming, the gh failure if it had one, and how much
    /// the filters are holding back — a shortened list must say it is one.
    fn discovered_header(&mut self, ui: &mut egui::Ui, hidden_old: usize, hidden_excluded: usize) {
        theme::section_title(ui, "DISCOVERED — HOME FOLDER + GITHUB");
        ui.horizontal(|ui| {
            if ui.small_button("⟳ refresh [R]").clicked() {
                self.refresh_repos(true);
            }
            if self.scanning_local || self.scanning_gh {
                ui.spinner();
                ui.label(theme::dim("scanning…"));
            } else if self.repo_cache_at > 0 {
                let now = chrono::Utc::now().timestamp();
                ui.label(theme::dim(&format!(
                    "updated {} ago",
                    discover::age_label(now, self.repo_cache_at)
                )));
            }
            let mut hidden = Vec::new();
            if hidden_old > 0 {
                hidden.push(format!(
                    "{hidden_old} inactive for over {} days",
                    self.settings.repo_max_age_days
                ));
            }
            if hidden_excluded > 0 {
                hidden.push(format!("{hidden_excluded} excluded"));
            }
            if !hidden.is_empty() {
                ui.label(theme::dim(&format!(
                    "· hidden: {} — edit in settings (Ctrl+,)",
                    hidden.join(", ")
                )));
            }
        });
        if let Some(e) = &self.gh_repos_error {
            ui.colored_label(theme::BAD, format!("gh: {e}"));
        }
        ui.add_space(2.0);
    }
}
