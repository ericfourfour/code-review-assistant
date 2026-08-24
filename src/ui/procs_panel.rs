//! What the app is running, and what it has paused.
//!
//! Three pieces of chrome, all serving the same promise: nothing this app
//! starts should be invisible, and nothing it stops should have to be taken on
//! trust.
//!
//! * [`nav_notice`] — the banner shown after leaving a page that had models
//!   working. It names each process by pid and only says "terminated" once
//!   that process has confirmed it, so the reviewer watches the kill land
//!   rather than reading a claim that it was requested.
//! * [`window`] — the ledger: every call this run has made, with its pid,
//!   session, elapsed time and spend, and a stop button per row.
//! * [`paused_row`] — the card a page shows in place of a call it lost, with
//!   the choice between continuing the same session and asking again.

use egui::RichText;

use crate::app::{CraApp, PausedCall};
use crate::procs::RunState;
use crate::settings::Settings;
use crate::ui::theme;

/// What the reviewer asked of a paused call.
#[derive(PartialEq, Eq)]
pub enum PausedAction {
    None,
    /// Continue the same CLI conversation.
    Resume,
    /// Start over on a new one.
    Restart,
}

/// The confirmation banner: what leaving the last page stopped.
///
/// It stays until dismissed rather than fading, because the one thing it is
/// for is being read. A toast that disappears while the reviewer is looking
/// somewhere else would leave exactly the doubt this is meant to remove.
pub fn nav_notice(app: &mut CraApp, ctx: &egui::Context) {
    let Some(notice) = &app.nav_notice else {
        return;
    };
    let settled = notice.all_confirmed();
    let mut dismiss = false;
    let mut go_back: Option<crate::app::Screen> = None;

    egui::TopBottomPanel::top("nav_notice").show(ctx, |ui| {
        ui.horizontal_wrapped(|ui| {
            // The mark is the whole point: a tick means every named process
            // has confirmed it is gone, and a spinner means it has not yet.
            if settled {
                ui.label(RichText::new("✔").color(theme::GOOD).strong());
            } else {
                ui.spinner();
            }
            ui.label(RichText::new(notice.headline()).strong().color(if settled {
                theme::GOOD
            } else {
                theme::WARN
            }));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Dismiss").clicked() {
                    dismiss = true;
                }
                if notice.resumable > 0
                    && ui
                        .small_button(format!("◀ Back to the {}", notice.left.label()))
                        .on_hover_text("returns without restarting anything")
                        .clicked()
                {
                    go_back = Some(notice.left);
                }
            });
        });
        for r in &notice.receipts {
            ui.horizontal_wrapped(|ui| {
                let done = r.confirmed();
                ui.label(
                    RichText::new(if done { "  ·" } else { "  ⟳" })
                        .monospace()
                        .color(if done { theme::TEXT_DIM } else { theme::WARN }),
                );
                ui.label(RichText::new(r.line()).monospace().small());
                ui.label(theme::dim(&format!("({})", r.owner.label())));
                match &r.session {
                    Some(id) => {
                        ui.label(theme::dim("· session"));
                        ui.label(
                            RichText::new(short(id))
                                .monospace()
                                .small()
                                .color(theme::ACCENT),
                        )
                        .on_hover_text(format!("session {id}\nresume picks this conversation up"));
                    }
                    None => {
                        ui.label(theme::dim(
                            "· no session id — it was stopped before reporting one",
                        ));
                    }
                }
            });
        }
        ui.add_space(2.0);
    });

    if let Some(screen) = go_back {
        app.nav_notice = None;
        app.goto(screen);
    } else if dismiss {
        app.nav_notice = None;
    }
}

/// The process ledger, opened from the top bar.
pub fn window(app: &mut CraApp, ctx: &egui::Context) {
    if !app.show_procs {
        return;
    }
    let mut open = true;
    // Collected rather than acted on inside the loop: the table is borrowed
    // while it is being drawn.
    let mut stop: Option<u64> = None;
    let mut stop_all = false;

    egui::Window::new("Model processes and sessions")
        .open(&mut open)
        .default_width(880.0)
        .default_height(460.0)
        .show(ctx, |ui| {
            let spent = app.procs.spent();
            ui.horizontal_wrapped(|ui| {
                let running = app.procs.running_total();
                theme::badge(
                    ui,
                    &format!("{running} RUNNING"),
                    if running > 0 {
                        theme::WARN
                    } else {
                        theme::TEXT_DIM
                    },
                );
                ui.label(theme::dim(&format!(
                    "{} finished this run",
                    app.procs.completed()
                )));
                // Per page, so "something is still running" can be traced to
                // the screen that started it.
                for owner in crate::procs::Owner::ALL {
                    let n = app.procs.running(owner);
                    if n > 0 {
                        ui.label(theme::dim(&format!("· {n} on the {}", owner.label())));
                    }
                }
                ui.separator();
                ui.label(theme::dim("spent this run"));
                ui.label(RichText::new(spend_label(&spent)).monospace().small());
                if let Some(f) = app.usage_fraction() {
                    ui.label(
                        RichText::new(format!("{:.0}% of limit", f * 100.0))
                            .small()
                            .color(if f >= 1.0 {
                                theme::BAD
                            } else if f > 0.8 {
                                theme::WARN
                            } else {
                                theme::TEXT_DIM
                            }),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(running > 0, egui::Button::new("⏹ Stop all"))
                        .on_hover_text("terminates every model process this app started")
                        .clicked()
                    {
                        stop_all = true;
                    }
                });
            });
            if let Some(why) = app.usage_block() {
                ui.colored_label(theme::BAD, why);
            }
            ui.separator();

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if app.procs.rows().is_empty() {
                        ui.label(theme::dim("no model calls yet this run"));
                    }
                    // Newest first: the question this window answers is almost
                    // always about what just happened.
                    for row in app.procs.rows().iter().rev() {
                        let snap = row.snapshot();
                        ui.horizontal_wrapped(|ui| {
                            let (mark, colour) = match &snap.state {
                                RunState::Starting => ("◌", theme::TEXT_DIM),
                                RunState::Running => ("●", theme::GOOD),
                                RunState::Stopping => ("◍", theme::WARN),
                                RunState::Done(e) if e.interrupted() => ("○", theme::WARN),
                                RunState::Done(_) => ("○", theme::TEXT_DIM),
                            };
                            ui.label(RichText::new(mark).monospace().color(colour));
                            ui.label(
                                RichText::new(format!("{:<9}", snap.state.label()))
                                    .monospace()
                                    .small()
                                    .color(colour),
                            );
                            ui.label(
                                RichText::new(&row.model)
                                    .monospace()
                                    .small()
                                    .color(theme::model_color(row.model_index)),
                            );
                            ui.label(theme::dim(row.owner.label()));
                            ui.label(RichText::new(snap.pid_label()).monospace().small());
                            match &snap.session {
                                Some(id) => {
                                    ui.label(
                                        RichText::new(short(id))
                                            .monospace()
                                            .small()
                                            .color(theme::ACCENT),
                                    )
                                    .on_hover_text(format!("session {id}"));
                                }
                                None => {
                                    ui.label(theme::dim("—")).on_hover_text(
                                        "this CLI reports its session id only when it finishes",
                                    );
                                }
                            }
                            ui.label(theme::dim(&format!("{}s", snap.elapsed.as_secs())));
                            if !snap.usage.is_silent() {
                                ui.label(
                                    RichText::new(spend_label(&snap.usage)).monospace().small(),
                                );
                            }
                            if snap.state.is_live()
                                && ui
                                    .small_button("⏹")
                                    .on_hover_text("terminate this process")
                                    .clicked()
                            {
                                stop = Some(row.id);
                            }
                        });
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new("    ").monospace().small());
                            ui.label(theme::dim(&row.what));
                            if let Some(a) = snap.activity_line() {
                                ui.label(theme::dim(&format!("· {a}")));
                            }
                        });
                    }

                    // Conversations an earlier run walked away from. The CLI still
                    // holds them; this is the only place their ids survive, and
                    // without it they would be unreachable rather than merely
                    // forgotten.
                    let earlier = app.earlier_paused_sessions();
                    if !earlier.is_empty() {
                        ui.add_space(8.0);
                        theme::section_title(ui, "PAUSED BY AN EARLIER RUN");
                        for row in &earlier {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new("⏸").monospace().color(theme::WARN));
                                ui.label(RichText::new(&row.model).monospace().small());
                                ui.label(theme::dim(&row.owner));
                                ui.label(
                                    RichText::new(short(&row.session))
                                        .monospace()
                                        .small()
                                        .color(theme::ACCENT),
                                )
                                .on_hover_text(format!("session {}", row.session));
                                ui.label(theme::dim(&row.what));
                                ui.label(theme::dim(&row.at));
                                match row.cost_usd {
                                    Some(usd) if usd > 0.0 => ui.label(
                                        RichText::new(format!("{} tok · ${usd:.4}", row.tokens))
                                            .monospace()
                                            .small(),
                                    ),
                                    _ => ui.label(
                                        RichText::new(format!("{} tok", row.tokens))
                                            .monospace()
                                            .small(),
                                    ),
                                };
                            });
                        }
                    }
                });
        });

    if stop_all {
        app.stop_all_models("stopped from the process ledger");
    } else if let Some(id) = stop {
        let receipts = app.procs.stop_one(id, "stopped from the process ledger");
        let n = receipts.len();
        if n > 0 {
            app.note(
                "procs",
                &format!("stopping {n} process(es) from the ledger"),
            );
        }
    }
    if !open {
        app.show_procs = false;
    }
}

/// One paused call, as the page that lost it shows it. Returns what the
/// reviewer asked for, if anything.
///
/// The two buttons are deliberately not the same offer. Resuming continues the
/// conversation the CLI still holds — the model keeps everything it had read
/// and worked out before the kill. Asking again throws that away and pays for
/// it a second time. Where there is no session id, only the second is possible
/// and the card says why rather than showing a button that would quietly do
/// something else.
pub fn paused_row(
    ui: &mut egui::Ui,
    call: &PausedCall,
    settings: &Settings,
    restart_label: &str,
) -> PausedAction {
    let mut action = PausedAction::None;
    let resumable = call.resumable(settings);
    egui::Frame::none()
        .fill(theme::RAISED)
        .inner_margin(egui::Margin::same(6.0))
        .rounding(4.0)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                theme::badge(ui, "PAUSED", theme::WARN);
                ui.label(RichText::new(&call.model).monospace().small());
                ui.label(theme::dim(&call.line()));
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(theme::dim(&call.reason));
                if !call.what.is_empty() {
                    ui.label(theme::dim(&format!("· {}", call.what)));
                }
                if !call.usage.is_silent() {
                    ui.label(RichText::new(spend_label(&call.usage)).monospace().small());
                }
            });
            ui.horizontal_wrapped(|ui| match &call.session {
                Some(id) => {
                    ui.label(theme::dim("session"));
                    ui.label(
                        RichText::new(short(id))
                            .monospace()
                            .small()
                            .color(theme::ACCENT),
                    )
                    .on_hover_text(format!("session {id}"));
                }
                None => {
                    ui.label(theme::dim(&call.session_line()));
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(resumable, egui::Button::new("▶ Resume this session"))
                    .on_hover_text(match resumable {
                        true => {
                            "continues the same conversation — the model keeps what it had read"
                        }
                        false => {
                            "no session to continue: this CLI never reported one, or it has \
                                  no resume command configured"
                        }
                    })
                    .clicked()
                {
                    action = PausedAction::Resume;
                }
                if ui
                    .button(restart_label)
                    .on_hover_text("starts a new conversation and pays for the work again")
                    .clicked()
                {
                    action = PausedAction::Restart;
                }
            });
        });
    action
}

/// The running/paused indicator for the top bar. Clicking it opens the ledger.
pub fn top_bar_badge(app: &mut CraApp, ui: &mut egui::Ui) {
    let running = app.procs.running_total();
    let paused = app.paused_here();
    let (text, colour) = match (running, paused) {
        (0, 0) => ("⏻ idle".to_string(), theme::TEXT_DIM),
        (0, p) => (format!("⏸ {p} paused"), theme::WARN),
        (r, 0) => (format!("● {r} running"), theme::GOOD),
        (r, p) => (format!("● {r} running · ⏸ {p} paused"), theme::GOOD),
    };
    if ui
        .add(egui::Button::new(RichText::new(text).small().color(colour)).frame(false))
        .on_hover_text("model processes and sessions — pids, session ids and what they spent")
        .clicked()
    {
        app.show_procs = !app.show_procs;
    }
    if let Some(f) = app.usage_fraction() {
        ui.label(
            RichText::new(format!("{:.0}%", f * 100.0))
                .small()
                .color(if f >= 1.0 {
                    theme::BAD
                } else if f > 0.8 {
                    theme::WARN
                } else {
                    theme::TEXT_DIM
                }),
        )
        .on_hover_text("of this run's usage limit");
    }
}

/// A session id at the length a human matches on: enough to recognise, short
/// enough to sit in a row. The whole id is always one hover away.
fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn spend_label(u: &crate::models::Usage) -> String {
    match u.cost_usd {
        Some(usd) => format!("{} tok · ${usd:.4}", u.tokens()),
        None if u.tokens() > 0 => format!("{} tok", u.tokens()),
        None => "unmeasured".to_string(),
    }
}
