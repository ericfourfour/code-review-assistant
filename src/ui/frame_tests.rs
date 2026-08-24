//! Headless frame tests.
//!
//! `egui::Context::run` drives a full layout pass with no window and no GPU,
//! so the chrome and the review screen can be laid out in a test and the
//! resulting state inspected. That covers two things unit tests cannot: that a
//! keypress reaches the handler it is advertised as reaching, and that a
//! layout is stable frame to frame.

use egui::{Event, Key, Modifiers, RawInput, Rect, Vec2};

use crate::app::{CraApp, Screen};
use crate::db::Db;
use crate::testkit::TempDir;

/// An app with its own database, parked on the review screen with no models
/// configured, so nothing spawns while frames are being laid out.
fn headless_app(tag: &str) -> (TempDir, CraApp) {
    let dir = TempDir::new(tag);
    let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
    let mut app = CraApp::with_db(db);
    app.settings.models.clear();
    (dir, app)
}

fn input(width: f32, height: f32) -> RawInput {
    RawInput {
        screen_rect: Some(Rect::from_min_size(Default::default(), Vec2::new(width, height))),
        ..Default::default()
    }
}

fn key_press(key: Key) -> Event {
    Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }
}

/// Lay out the persistent chrome plus the current screen, exactly as
/// `eframe::App::update` does, and report the height left for the content.
fn lay_out(app: &mut CraApp, ctx: &egui::Context, raw: RawInput) -> f32 {
    let mut central_height = 0.0;
    let _ = ctx.run(raw, |ctx| {
        crate::ui::chrome::top_bar(app, ctx);
        crate::ui::chrome::hotkey_bar(app, ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            central_height = ui.available_height();
            ui.label("content");
        });
    });
    central_height
}

/// Signature of everything painted this frame, for comparing one frame to the
/// next. Clip rectangles move when the layout does, so a stable signature
/// means the frame was laid out identically.
fn paint_signature(app: &mut CraApp, ctx: &egui::Context, raw: RawInput) -> (usize, String) {
    let out = ctx.run(raw, |ctx| {
        crate::ui::chrome::top_bar(app, ctx);
        crate::ui::chrome::hotkey_bar(app, ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label("content");
        });
    });
    let clips: Vec<_> = out.shapes.iter().map(|s| s.clip_rect).collect();
    (out.shapes.len(), format!("{clips:?}"))
}

/// The chrome must lay out the same way on every repaint, at any width and
/// regardless of how long the status line is — it repaints on every pointer
/// move, so anything that shifts frame to frame shows up as jitter.
///
/// Note this does *not* reproduce the flicker reported against the previous
/// wrapped-row layout: that layout passes this test too, headless. It pins the
/// invariant going forward rather than proving the original fix.
#[test]
fn the_chrome_lays_out_identically_on_every_repaint() {
    let (_dir, mut app) = headless_app("stable");
    app.screen = Screen::Review;
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);

    for width in [1100.0, 1280.0, 1480.0, 1920.0] {
        for status in ["", "committed 3f9a21b src/app.rs", &"x".repeat(4000)] {
            app.status = status.to_string();
    // The first frame after a change stabilizes cached panel sizes; the
            // frames after it are the ones a user actually watches.
            let _ = paint_signature(&mut app, &ctx, input(width, 900.0));
            let first = paint_signature(&mut app, &ctx, input(width, 900.0));
            for _ in 0..3 {
                let next = paint_signature(&mut app, &ctx, input(width, 900.0));
                assert_eq!(
                    first, next,
                    "chrome shifted between repaints at {width}px with a {}-char status",
                    status.len()
                );
            }
        }
    }
}

/// Moving the pointer may change hover decoration — a highlight, or the
/// tooltip that a truncated status line shows — but it must never move the
/// layout, which would push the content below it around.
#[test]
fn moving_the_pointer_does_not_move_the_layout() {
    let (_dir, mut app) = headless_app("pointer");
    app.screen = Screen::Review;
    app.status = "x".repeat(4000);
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);

    let baseline = lay_out(&mut app, &ctx, input(1480.0, 900.0));
    for x in [10.0, 400.0, 900.0, 1400.0] {
        for y in [12.0, 450.0, 888.0] {
            let mut raw = input(1480.0, 900.0);
            raw.events.push(Event::PointerMoved(egui::pos2(x, y)));
            let height = lay_out(&mut app, &ctx, raw);
            assert!(
                (baseline - height).abs() < 0.01,
                "pointer at ({x}, {y}) resized the chrome: {baseline} then {height}"
            );
        }
    }
}

/// Lay out the evaluation screen itself, delivering `events` first.
fn eval_frame(app: &mut CraApp, ctx: &egui::Context, events: Vec<Event>) {
    let mut raw = input(1480.0, 900.0);
    raw.events = events;
    let _ = ctx.run(raw, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            app.ui_eval(ctx, ui);
        });
    });
}

/// The evaluation page is reachable from anywhere and returns to wherever it
/// was opened from. It reads history and touches nothing, so it must not cost
/// the reviewer their place in a review to look at it.
#[test]
fn ctrl_e_opens_the_evaluation_page_and_esc_returns_to_where_it_came_from() {
    let (_dir, mut app) = headless_app("eval_nav");
    app.screen = Screen::Review;
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);

    let mut raw = input(1480.0, 900.0);
    raw.events = vec![Event::Key {
        key: Key::E,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::CTRL,
    }];
    let _ = ctx.run(raw, |ctx| app.global_hotkeys(ctx));
    assert_eq!(app.screen as u8, Screen::Eval as u8);

    let mut raw = input(1480.0, 900.0);
    raw.events = vec![key_press(Key::Escape)];
    let _ = ctx.run(raw, |ctx| app.global_hotkeys(ctx));
    assert_eq!(app.screen as u8, Screen::Review as u8, "Esc must return to the review");
}

/// The page has to lay out with a real history behind it, not just when empty:
/// every bar, the head-to-head row and the verdict mix only exist once there
/// are decisions, and each of them paints by hand into an allocated rect.
#[test]
fn the_evaluation_page_lays_out_with_real_history_and_stays_put() {
    let (_dir, mut app) = headless_app("eval_layout");
    let session = app.db.new_session("C:/work/widgets", "branch", "feature", "main");
    for (line, winner) in [(2, "claude"), (6, "codex"), (9, "claude+human-edited"), (12, "original")]
    {
        for model in ["claude", "codex"] {
            app.db.log_suggestion(&crate::db::SuggestionRecord {
                session_id: session,
                file: "src/lib.rs",
                line_start: line,
                line_end: line,
                model,
                action: Some("rewrite"),
                comment: Some("Counts retries."),
                justification: None,
                latency_ms: 900,
                error: None,
                evidence: None,
                usage: Some(crate::models::Usage {
                    input_tokens: 1200,
                    output_tokens: 90,
                    cache_read_tokens: 8000,
                    cost_usd: Some(0.004),
                }),
                cost: Some((0.004, false)),
                follow_up_id: None,
                round: 1,
            });
        }
        app.db.log_decision(&crate::db::DecisionRecord {
            session_id: session,
            file: "src/lib.rs",
            line_start: line,
            line_end: line,
            original: "    // a",
            action: "rewrite",
            final_text: "    // Counts retries.",
            source: winner,
            human_edited: false,
            committed: false,
            commit_sha: None,
            justification: None,
            unit_json: None,
            blinded: true,
        });
    }
    app.screen = Screen::Eval;
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);

    eval_frame(&mut app, &ctx, vec![]);
    let board = app.eval.as_ref().expect("the page loads its aggregate on first frame");
    assert_eq!(board.contests, 4);
    assert_eq!(board.head_to_head.len(), 1, "two models met on every contest");

    // Repaints must be identical: this screen redraws on every pointer move,
    // and a bar that re-measures itself each frame reads as a flicker.
    let signature = |app: &mut CraApp| {
        let out = ctx.run(input(1480.0, 900.0), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                app.ui_eval(ctx, ui);
            });
        });
        (out.shapes.len(), format!("{:?}", out.shapes.iter().map(|s| s.clip_rect).collect::<Vec<_>>()))
    };
    let first = signature(&mut app);
    assert_eq!(first, signature(&mut app), "the evaluation page shifted between repaints");
}

/// `B` toggles the blinded-only filter and the aggregate follows it. The
/// filter changes what every number on the page means, so a stale board behind
/// a moved checkbox would be actively misleading.
#[test]
fn toggling_the_blinded_filter_rebuilds_the_aggregate() {
    let (_dir, mut app) = headless_app("eval_filter");
    let session = app.db.new_session("C:/work/widgets", "branch", "feature", "main");
    for (line, blinded) in [(2u32, true), (6, false)] {
        app.db.log_suggestion(&crate::db::SuggestionRecord {
            session_id: session,
            file: "src/lib.rs",
            line_start: line,
            line_end: line,
            model: "claude",
            action: Some("rewrite"),
            comment: Some("text"),
            justification: None,
            latency_ms: 100,
            error: None,
            evidence: None,
            usage: None,
            cost: None,
            follow_up_id: None,
            round: 1,
        });
        app.db.log_decision(&crate::db::DecisionRecord {
            session_id: session,
            file: "src/lib.rs",
            line_start: line,
            line_end: line,
            original: "    // a",
            action: "rewrite",
            final_text: "    // b",
            source: "claude",
            human_edited: false,
            committed: false,
            commit_sha: None,
            justification: None,
            unit_json: None,
            blinded,
        });
    }
    app.screen = Screen::Eval;
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);

    eval_frame(&mut app, &ctx, vec![]);
    assert_eq!(app.eval.as_ref().unwrap().contests, 1, "blinded only, by default");

    eval_frame(&mut app, &ctx, vec![key_press(Key::B)]);
    assert!(!app.eval_filter.blinded_only);
    assert_eq!(app.eval.as_ref().unwrap().contests, 2, "the board follows the filter");
}

/// Lay out the review screen itself, delivering `events` first.
fn review_frame(app: &mut CraApp, ctx: &egui::Context, events: Vec<Event>) {
    let mut raw = input(1480.0, 900.0);
    raw.events = events;
    let _ = ctx.run(raw, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            app.ui_review(ctx, ui);
        });
    });
}

/// Lay out the review screen and report `(height used, height offered)`.
fn review_extent(app: &mut CraApp, ctx: &egui::Context) -> (f32, f32) {
    let (mut used, mut offered) = (0.0, 0.0);
    let _ = ctx.run(input(1480.0, 900.0), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            let top = ui.cursor().top();
            offered = ui.available_height();
            app.ui_review(ctx, ui);
            // The cursor, not `min_rect`: a panel pre-expands its min_rect to
            // its own height, so only the cursor says what was really used.
            used = ui.cursor().top() - top;
        });
    });
    (used, offered)
}

/// The review screen must fit the window it is given and use all of it: a
/// long unit scrolls inside its pane rather than pushing the button bar off
/// the bottom, and a short one does not leave the panes stranded at some
/// fixed height with dead space below.
#[test]
fn the_review_screen_fills_its_window_and_never_overflows_it() {
    let (_dir, mut app) = review_app("extent");
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);
    app.start_review(&ctx, 0);

    let long: String = (0..500).map(|i| format!("    step{i}();\n")).collect();
    for (label, text) in [("a short unit", "// one line".to_string()), ("a 500-line unit", long)] {
        app.original_display = text.clone();
        app.editor = text;
        // The first frame after a change measures; the ones after it are what
        // a user actually sees.
        let _ = review_extent(&mut app, &ctx);
        let (used, offered) = review_extent(&mut app, &ctx);
        assert!(used <= offered + 0.5, "{label} overflowed: used {used} of {offered}");
        assert!(
            used > offered - 6.0,
            "{label} left {} px of the window empty",
            offered - used
        );
    }
}

/// An app parked on the review screen with a real two-comment diff to review.
fn review_app(tag: &str) -> (TempDir, CraApp) {
    use crate::testkit::TempRepo;

    let dir = TempDir::new(tag);
    let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
    let mut app = CraApp::with_db(db);
    app.settings.models.clear();

    let repo = TempRepo::new(tag);
    repo.write("src/lib.rs", "fn main() {}\n");
    repo.commit("base");
    repo.git(&["checkout", "-b", "feature"]);
    repo.write(
        "src/lib.rs",
        concat!(
            "fn main() {\n",
            "    // Increment the counter by one\n",
            "    counter += 1;\n",
            "    // Reset the counter to zero\n",
            "    counter = 0;\n",
            "}\n",
        ),
    );
    repo.commit("add counter");

    let diff = crate::gitio::review_diff(&repo.path(), "main", 12).expect("diff");
    let extracted = crate::comments::extract_units(&crate::diffparse::parse(&diff), 12);
    let files = extracted
        .into_iter()
        .map(|(path, units)| {
            crate::review::ReviewFile::new(
                path,
                units.into_iter().map(crate::units::ReviewUnit::Comment).collect(),
            )
        })
        .collect();
    app.repo = Some(crate::app::RepoCtx {
        path: repo.path(),
        name: "test-repo".into(),
        default_branch: "main".into(),
    });
    app.plan = Some(crate::review::ReviewPlan {
        session_id: 1,
        ref_kind: crate::review::RefKind::Branch,
        ref_name: "feature".into(),
        base_ref: "main".into(),
        files,
        file_idx: 0,
        unit_idx: 0,
        decided_total: 0,
        skipped_decided: 0,
    });
    (dir, app)
}

#[test]
fn review_hotkeys_reach_their_handlers() {
    let (_dir, mut app) = review_app("hotkeys");
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);
    app.start_review(&ctx, 0);

    // D clears the editor for deletion...
    review_frame(&mut app, &ctx, vec![key_press(Key::D)]);
    assert!(app.editor.is_empty(), "D should stage a deletion");
    assert_eq!(app.chosen, Some(crate::review::Choice::Delete));

    // ...and K puts the original back.
    review_frame(&mut app, &ctx, vec![key_press(Key::K)]);
    assert_eq!(app.editor, app.original_display);
    assert_eq!(app.chosen, Some(crate::review::Choice::KeepOriginal));

    // N moves to the next comment in the file.
    let first = app.original_display.clone();
    review_frame(&mut app, &ctx, vec![key_press(Key::N)]);
    assert_eq!(app.original_display, "// Reset the counter to zero");

    // P goes back to it.
    review_frame(&mut app, &ctx, vec![key_press(Key::P)]);
    assert_eq!(app.original_display, first, "P should step back");
    assert!(app.chosen.is_none(), "re-entering a comment starts from a clean slate");

    // F focuses the follow-up box...
    review_frame(&mut app, &ctx, vec![key_press(Key::F)]);
    assert!(ctx.memory(|m| m.focused().is_some()), "F should put focus in the follow-up box");

    // ...and once it has focus, letters are text, not commands. Without this
    // guard, typing "delete this line?" into the box would delete the comment,
    // skip to the next one, and keep the original along the way.
    review_frame(&mut app, &ctx, vec![key_press(Key::D)]);
    assert!(app.chosen.is_none(), "D was treated as a hotkey while typing");
    assert_eq!(app.original_display, first, "the review moved on while typing");
}

/// X leaves the review for the summary before every unit is decided — the door
    /// to the whole-branch review and the follow-up notes must not require finishing the
/// review — and the plan keeps its place for coming back.
#[test]
fn x_ends_the_session_early_and_keeps_the_plans_place() {
    let (_dir, mut app) = review_app("end_early");
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);
    app.start_review(&ctx, 0);

    review_frame(&mut app, &ctx, vec![key_press(Key::X)]);
    assert_eq!(app.screen as u8, Screen::Summary as u8);
    let plan = app.plan.as_ref().expect("the plan survives an early exit");
    assert_eq!(plan.decided_total, 0);
    assert!(plan.total_units() > 0, "units are still waiting to be resumed");
}

/// A REVISE card previews the proposal as a diff, so the review screen has to
/// lay one out inside the narrow candidate column without panicking and
/// without pushing the panes below it off the window.
#[test]
fn a_revise_candidate_lays_out_as_a_diff_inside_its_card() {
    use crate::app::CandidateState;
    use crate::models::{Action, Suggestion};

    let (_dir, mut app) = review_app("revise");
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);
    app.start_review(&ctx, 0);

    // A code unit, so the preview takes the diff path rather than the plain
    // replacement text a comment rewrite shows.
    let unit = app.current_unit().expect("a unit to review");
    let code = crate::codeunits::CodeUnit {
        file: unit.file().to_string(),
        lang: unit.lang().to_string(),
        start_line: unit.start_line(),
        end_line: unit.end_line(),
        raw_lines: vec!["    counter += 1;".into(), "    log(counter);".into()],
        changed_lines: vec![unit.start_line()],
        scope: Some("fn main()".into()),
        context: unit.context().to_string(),
        hunk_header: unit.hunk_header().to_string(),
    };
    if let Some(p) = app.plan.as_mut() {
        p.files[0].units[0] = crate::units::ReviewUnit::Code(code);
    }
    app.enter_unit(&ctx);
    app.candidates = vec![CandidateState::Ready(Suggestion {
        action: Action::Rewrite,
        comment: "    counter += 1;\n    log(counter, \"unit\");\n".into(),
        justification: "the log call needs a label".into(),
        evidence: Vec::new(),
        latency_ms: 12,
    })];

    let _ = review_extent(&mut app, &ctx);
    let (used, offered) = review_extent(&mut app, &ctx);
    assert!(used <= offered + 0.5, "the diff preview overflowed: used {used} of {offered}");

        // And the diff itself says what would change, not just what would be applied.
    let rows = crate::ui::code::diff_rows(
        &app.original_display,
        "    counter += 1;\n    log(counter, \"unit\");",
    );
    let shape: Vec<String> =
        rows.iter().map(|r| format!("{}{}", r.gutter.trim_end(), r.text)).collect();
    assert_eq!(
        shape,
        vec!["    counter += 1;", "-    log(counter);", "+    log(counter, \"unit\");"]
    );
}

/// The follow-up screen lays out a populated backlog next to a live
/// transcript without panicking and identically on every repaint — its
/// per-note frames, collapsers and scroll areas all carry egui ids that must
/// not clash however many notes are on screen.
#[test]
fn the_followup_screen_lays_out_with_notes_and_a_transcript() {
    let (_dir, mut app) = review_app("followup");
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);

    let repo = app.repo.as_ref().expect("repo").path.clone();
    for text in ["extract the retry loop", "the error type swallows its cause"] {
        app.db.log_note(1, &repo, "src/lib.rs", 2, 3, "    // a comment\n    call();", text);
    }
    app.open_followup();
    assert_eq!(app.notes.len(), 2);
    app.notes[0].checked = true;
    app.fix_convo = vec![crate::models::Turn {
        prompt: "fix these".into(),
        reply: "extracted the loop into retry_with_backoff().".into(),
    }];

    let mut frames = Vec::new();
    for _ in 0..3 {
        let out = ctx.run(input(1480.0, 900.0), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                app.ui_followup(ctx, ui);
            });
        });
        let clips: Vec<_> = out.shapes.iter().map(|s| s.clip_rect).collect();
        frames.push((out.shapes.len(), format!("{clips:?}")));
    }
    assert_eq!(frames[1], frames[2], "the follow-up screen shifted between repaints");
}

/// The picker's rows are layout jobs rather than formatted strings — the
/// `+`/`−` counts carry the diff colours — so they have to survive a real
/// layout pass, and lay out the same way twice like the rest of the chrome.
#[test]
fn the_file_picker_lays_out_its_rows() {
    let (_dir, mut app) = review_app("picker");
    app.screen = Screen::FilePicker;
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);

    let mut frames = Vec::new();
    for _ in 0..3 {
        let out = ctx.run(input(1480.0, 900.0), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                app.ui_file_picker(ctx, ui);
            });
        });
        let clips: Vec<_> = out.shapes.iter().map(|s| s.clip_rect).collect();
        frames.push((out.shapes.len(), format!("{clips:?}")));
    }
    assert!(frames[0].0 > 0, "the picker painted nothing");
    assert_eq!(frames[1], frames[2], "the picker shifted between repaints");
}
