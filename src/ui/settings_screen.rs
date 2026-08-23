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
        ui.label(theme::dim("stored in the local sqlite database; saved on close"));
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .id_salt("settings_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| self.settings_body(ui));
    }

    fn settings_body(&mut self, ui: &mut egui::Ui) {

        theme::section_title(ui, "REVIEWER MODELS");
        ui.label(theme::dim(
            "A template with no {prompt} token gets the prompt piped on stdin — preferred, since it has no length limit. Use {prompt} only for CLIs that cannot read stdin: as an argument the prompt is capped at ~32k characters, and .cmd shims (npm-installed CLIs) reject multi-line values outright. The resume template continues a conversation and must contain {session}; the session key names the JSON field the CLI reports its session id in, and is left empty for CLIs that accept an id we generate instead. CLIs must be installed and authenticated.",
        ));
        ui.add_space(4.0);
        ui.label(theme::dim(
            "Every CLI is started in the repository being reviewed, and the shipped templates \
carry the flags that let it read that repository — a comment often cannot be judged without \
looking past its own hunk. The flags differ per CLI: claude takes a tool allowlist, codex a \
read-only sandbox, and agy the {repo} token, which names the repository as its workspace \
(--add-dir) because it will not read a directory it was merely started in. agy also takes \
{cli_home}: its permissions live in a settings file rather than in flags, so it is given a \
home this app owns, granting it this repository and nothing else — your own ~/.gemini is left \
alone. It has no shell there, only file reads and its own search, so a comment whose \
justification would need git history is one it answers without. A hand-written \
template that drops them leaves that model reviewing on the diff alone. Reading the \
repository takes longer than answering from the hunk: raise the model timeout below rather \
than the other way round.",
        ));
        ui.add_space(4.0);

        let mut remove: Option<usize> = None;
        let n_models = self.settings.models.len();
        for i in 0..n_models {
            let color = theme::model_color(i);
            egui::Frame::group(ui.style())
                .fill(theme::PANEL)
                .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(55)))
                .inner_margin(egui::Margin::same(8.0))
                .show(ui, |ui| {
                    let m = &mut self.settings.models[i];
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut m.enabled, "");
                        ui.label(
                            RichText::new(format!("[{}]", i + 1)).monospace().strong().color(color),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut m.name)
                                .desired_width(140.0)
                                .hint_text("name")
                                .font(egui::TextStyle::Monospace),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✕ remove").clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                    ui.add_space(4.0);

                    field(ui, "command", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut m.command)
                                .desired_width(f32::INFINITY)
                                .hint_text("mycli --print - --dir {repo}")
                                .font(egui::TextStyle::Monospace),
                        );
                    });
                    field(ui, "model", |ui| {
                        let presets = crate::settings::model_presets(&m.command);
                        ui.add(
                            egui::TextEdit::singleline(&mut m.model)
                                .desired_width(260.0)
                                .hint_text("(CLI default)")
                                .font(egui::TextStyle::Monospace),
                        );
                        if !presets.is_empty() {
                            egui::ComboBox::from_id_salt(("model_preset", i))
                                .selected_text("presets ▾")
                                .width(210.0)
                                .show_ui(ui, |ui| {
                                    for p in presets {
                                        if ui.selectable_label(m.model == *p, *p).clicked() {
                                            m.model = (*p).to_string();
                                        }
                                    }
                                    if ui.selectable_label(m.model.is_empty(), "(CLI default)").clicked() {
                                        m.model.clear();
                                    }
                                });
                        }
                        ui.label(theme::dim("flag"));
                        ui.add(
                            egui::TextEdit::singleline(&mut m.model_flag)
                                .desired_width(90.0)
                                .font(egui::TextStyle::Monospace),
                        );
                    });
                    field(ui, "effort", |ui| {
                        let presets = crate::settings::effort_presets(&m.command);
                        ui.add(
                            egui::TextEdit::singleline(&mut m.effort)
                                .desired_width(260.0)
                                .hint_text("(CLI default)")
                                .font(egui::TextStyle::Monospace),
                        );
                        if !presets.is_empty() {
                            egui::ComboBox::from_id_salt(("effort_preset", i))
                                .selected_text("presets ▾")
                                .width(210.0)
                                .show_ui(ui, |ui| {
                                    for p in presets {
                                        if ui.selectable_label(m.effort == *p, *p).clicked() {
                                            m.effort = (*p).to_string();
                                        }
                                    }
                                    if ui
                                        .selectable_label(m.effort.is_empty(), "(CLI default)")
                                        .clicked()
                                    {
                                        m.effort.clear();
                                    }
                                });
                        }
                        ui.label(theme::dim("flag"));
                        ui.add(
                            egui::TextEdit::singleline(&mut m.effort_flag)
                                .desired_width(90.0)
                                .font(egui::TextStyle::Monospace),
                        );
                    });
                    field(ui, "resume", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut m.resume_command)
                                .desired_width(f32::INFINITY)
                                .hint_text("mycli --resume {session}  (empty = no follow-ups)")
                                .font(egui::TextStyle::Monospace),
                        );
                    });
                    field(ui, "session key", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut m.session_key)
                                .desired_width(200.0)
                                .hint_text("(we generate the id)")
                                .font(egui::TextStyle::Monospace),
                        );
                        let explain = if m.session_key.trim().is_empty() {
                            if m.command.contains("{session}") {
                                "we mint a UUID and pass it in the command"
                            } else {
                                "no session — add {session} to the command, or name the CLI's id field"
                            }
                        } else {
                            "JSON field the CLI reports its session id in"
                        };
                        ui.label(theme::dim(explain));
                    });
                    field(ui, "co-author", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut m.coauthor)
                                .desired_width(f32::INFINITY)
                                .hint_text("Name <email>")
                                .font(egui::TextStyle::Monospace),
                        );
                    });
                });
            ui.add_space(4.0);
        }
        if let Some(i) = remove {
            self.settings.models.remove(i);
        }
        if ui.button("+ add model").clicked() {
            self.settings.models.push(ModelSlot {
                name: "model".into(),
                command: "mycli --print -".into(),
                coauthor: "Model <model@example.com>".into(),
                enabled: true,
                model: String::new(),
                model_flag: "--model".into(),
                effort: String::new(),
                effort_flag: "--effort".into(),
                resume_command: String::new(),
                session_key: String::new(),
            });
        }

        ui.add_space(8.0);
        theme::section_title(ui, "GIT / GITHUB");
        egui::Grid::new("git_grid").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
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
        egui::Grid::new("review_grid").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label(theme::dim("model timeout (s)"));
            ui.add(egui::DragValue::new(&mut self.settings.model_timeout_secs).range(5..=900));
            ui.end_row();
            ui.label(theme::dim("context lines"));
            ui.add(egui::DragValue::new(&mut self.settings.context_lines).range(2..=60));
            ui.end_row();
            ui.label(theme::dim("blind review"));
            ui.checkbox(&mut self.settings.blind_review, "hide model names until you choose");
            ui.end_row();
            ui.label(theme::dim("review"));
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.settings.review_comments, "comments");
                ui.checkbox(&mut self.settings.review_code, "code");
            });
            ui.end_row();
        });
        ui.label(theme::dim(
            "Blind review also shuffles the candidates per comment. Every review is also a \
labelled example, and knowing which model wrote which suggestion while you choose biases that \
label — turn it off only if you are not measuring the models against each other.",
        ));
        ui.add_space(4.0);
        ui.label(theme::dim(
            "Code review walks every changed cluster of code — as the whole enclosing \
function or class where the language allows, as the surrounding hunk where it does not. \
Comment runs sitting between changed code lines are judged with that code rather than \
separately. Turning a kind off here narrows every future session to the other.",
        ));

        ui.add_space(8.0);
        theme::section_title(ui, "VALIDATION");
        egui::Grid::new("check_grid").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label(theme::dim("check command"));
            ui.add(
                egui::TextEdit::singleline(&mut self.settings.check_command)
                    .desired_width(f32::INFINITY)
                    .hint_text("cargo check   (empty = no validation)")
                    .font(egui::TextStyle::Monospace),
            );
            ui.end_row();
            ui.label(theme::dim("check timeout (s)"));
            ui.add(egui::DragValue::new(&mut self.settings.check_timeout_secs).range(5..=1800));
            ui.end_row();
            ui.label(theme::dim("comment edits too"));
            ui.checkbox(
                &mut self.settings.validate_comment_edits,
                "also run the check after comment-only edits",
            );
            ui.end_row();
        });
        ui.label(theme::dim(
            "Run in the repository after each applied edit, whitespace-tokenized, no shell — \
the repo's own fast check (cargo check, tsc --noEmit, go build, pytest -x …). An edit whose \
check fails is reverted on the spot and the failure shown, so a bad model rewrite can never \
walk the review onto a broken tree. The check runs synchronously: pick a fast one.",
        ));

        ui.add_space(10.0);
        if ui.button(RichText::new("Save and close  [Ctrl+S / Esc]").strong()).clicked() {
            self.close_settings();
        }
    }
}

/// One labelled row inside a model card: fixed-width label, the rest for the
/// editors. Keeps every field on the card the same width regardless of how
/// long its label is.
fn field(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized([84.0, 18.0], egui::Label::new(theme::dim(label)));
        add(ui);
    });
}
