//! Persistent chrome: breadcrumb top bar, hotkey bottom bar, session side panel.

use egui::RichText;

use crate::app::{CandidateState, CraApp, Screen};
use crate::ui::theme;

pub fn top_bar(app: &mut CraApp, ctx: &egui::Context) {
    egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("CRA").strong().color(theme::ACCENT).monospace());
            ui.separator();
            let crumb = |ui: &mut egui::Ui, s: &str, active: bool| {
                if active {
                    ui.label(RichText::new(s).strong());
                } else {
                    ui.label(theme::dim(s));
                }
            };
            match &app.repo {
                Some(r) => crumb(ui, &r.name, true),
                None => crumb(ui, "repo?", false),
            }
            ui.label(theme::dim("▸"));
            match &app.plan {
                Some(p) => crumb(ui, &format!("{} [{}]", p.ref_name, p.ref_kind.label()), true),
                None => crumb(ui, "branch/PR?", false),
            }
            ui.label(theme::dim("▸"));
            match app.plan.as_ref().and_then(|p| p.current().map(|(f, u)| (f.path.clone(), u.start_line))) {
                Some((f, l)) => crumb(ui, &format!("{f}:{l}"), app.screen == Screen::Review),
                None => crumb(ui, "file?", false),
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let name = match app.screen {
                    Screen::RepoPicker => "PICK REPOSITORY",
                    Screen::RefPicker => "PICK BRANCH / PR",
                    Screen::FilePicker => "PICK FILE",
                    Screen::Review => "REVIEW",
                    Screen::Summary => "SUMMARY",
                    Screen::Settings => "SETTINGS",
                };
                ui.label(RichText::new(name).small().strong().color(theme::ACCENT));
                if let Some(p) = &app.plan {
                    ui.separator();
                    ui.label(theme::dim(&format!(
                        "{}/{} comments decided",
                        p.decided_total,
                        p.total_units()
                    )));
                }
            });
        });
    });
}

pub fn hotkey_bar(app: &mut CraApp, ctx: &egui::Context) {
    // Fixed height + a non-wrapping row: a wrapped row containing a
    // right-to-left sub-layout re-measures itself every frame, so the panel
    // oscillated between one and two rows and the bar flickered on any
    // repaint — including plain mouse movement.
    egui::TopBottomPanel::bottom("hotkey_bar").exact_height(24.0).show(ctx, |ui| {
        ui.horizontal_centered(|ui| {
            let k = theme::kbd;
            match app.screen {
                Screen::RepoPicker => {
                    k(ui, "↑↓", "select");
                    k(ui, "Enter", "open repo");
                    k(ui, "X", "forget");
                }
                Screen::RefPicker => {
                    k(ui, "Tab", "branches ⇄ PRs");
                    k(ui, "↑↓", "select");
                    k(ui, "Enter", "review");
                    k(ui, "W", "review working tree");
                    k(ui, "R", "refresh");
                    k(ui, "Esc", "back");
                }
                Screen::FilePicker => {
                    k(ui, "↑↓", "select");
                    k(ui, "Enter", "start at file");
                    k(ui, "S", "start full review");
                    k(ui, "Esc", "back");
                }
                Screen::Review => {
                    k(ui, "1/2/3", "pick");
                    k(ui, "K", "keep");
                    k(ui, "D", "delete");
                    k(ui, "E", "edit");
                    k(ui, "F", "follow-up");
                    k(ui, "R", "re-run");
                    k(ui, "P", "prev");
                    k(ui, "N", "skip");
                    k(ui, "Ctrl+S", "save");
                    k(ui, "Ctrl+Enter", "commit");
                    k(ui, "Esc", "files");
                }
                Screen::Summary => {
                    k(ui, "F", "files");
                    k(ui, "B", "branches/PRs");
                    k(ui, "Esc", "repos");
                }
                Screen::Settings => {
                    k(ui, "Ctrl+S", "save + close");
                    k(ui, "Esc", "save + close");
                }
            }
            k(ui, "Ctrl+,", "settings");
            k(ui, "Ctrl+Q", "quit");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(theme::dim(&app.status)).truncate());
            });
        });
    });
}

pub fn side_panel(app: &mut CraApp, ctx: &egui::Context) {
    egui::SidePanel::right("side_panel")
        .default_width(300.0)
        .min_width(240.0)
        .show(ctx, |ui| {
            theme::section_title(ui, "SESSION");
            egui::Grid::new("session_grid").num_columns(2).spacing([8.0, 1.0]).show(ui, |ui| {
                if let Some(r) = &app.repo {
                    ui.label(theme::dim("repo"));
                    ui.label(RichText::new(&r.name).monospace().small());
                    ui.end_row();
                }
                if let Some(p) = &app.plan {
                    ui.label(theme::dim("ref"));
                    ui.label(RichText::new(&p.ref_name).monospace().small());
                    ui.end_row();
                    ui.label(theme::dim("base"));
                    ui.label(RichText::new(&p.base_ref).monospace().small());
                    ui.end_row();
                    ui.label(theme::dim("session"));
                    ui.label(RichText::new(format!("#{}", p.session_id)).monospace().small());
                    ui.end_row();
                }
            });

            if let Some(p) = &app.plan {
                ui.add_space(6.0);
                theme::section_title(ui, "PROGRESS");
                let (fi, ft) = p.file_progress();
                let file_name = p
                    .files
                    .get(p.file_idx)
                    .map(|f| f.path.clone())
                    .unwrap_or_else(|| "—".into());
                ui.label(theme::dim(&format!("file: {file_name}")));
                ui.add(
                    egui::ProgressBar::new(if ft == 0 { 1.0 } else { fi as f32 / ft as f32 })
                        .text(RichText::new(format!("{fi}/{ft}")).small()),
                );
                let total = p.total_units();
                ui.label(theme::dim("branch"));
                ui.add(
                    egui::ProgressBar::new(if total == 0 {
                        1.0
                    } else {
                        p.decided_total as f32 / total as f32
                    })
                    .text(RichText::new(format!("{}/{}", p.decided_total, total)).small()),
                );
            }

            if app.screen == Screen::Review {
                ui.add_space(6.0);
                theme::section_title(ui, "MODELS");
                // While blinding is on this panel must not pair a name with a
                // verdict, or the cards on the left are blinded for nothing.
                // That means the label AND the dot colour: colour is branded
                // by slot position (see theme::model_color), so leaving it
                // keyed to the real slot index would leak identity through
                // the palette even with the name itself hidden.
                let hidden = app.names_hidden();
                let order = app.candidate_order();
                for (i, slot) in app.settings.models.iter().enumerate() {
                    let pos = order.iter().position(|&s| s == i).unwrap_or(i);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("●")
                                .color(theme::model_color(if hidden { pos } else { i }))
                                .monospace(),
                        );
                        ui.label(RichText::new(app.slot_label(i, pos)).monospace().small());
                        if !slot.model.trim().is_empty() && !hidden {
                            ui.label(theme::dim(slot.model.trim()));
                        }
                        match app.candidates.get(i) {
                            Some(CandidateState::Pending) => {
                                ui.spinner();
                            }
                            Some(CandidateState::Ready(s)) if hidden => {
                                ui.label(theme::dim(&format!("answered · {} ms", s.latency_ms)));
                            }
                            Some(CandidateState::Ready(s)) => {
                                theme::badge(ui, s.action.label(), theme::action_color(s.action));
                                ui.label(theme::dim(&format!("{} ms", s.latency_ms)));
                            }
                            Some(CandidateState::Failed(_)) => {
                                theme::badge(ui, "FAIL", theme::BAD);
                            }
                            Some(CandidateState::Disabled) | None => {
                                ui.label(theme::dim("off"));
                            }
                        }
                    });
                }
            }

            ui.add_space(6.0);
            theme::section_title(ui, "ACTIVITY");
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for (ts, line) in &app.log_lines {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(ts).monospace().small().color(theme::TEXT_DIM));
                            ui.label(RichText::new(line).small());
                        });
                    }
                });
        });
}
