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
        screen_rect: Some(Rect::from_min_size(
            Default::default(),
            Vec2::new(width, height),
        )),
        ..Default::default()
    }
}

fn key_press(key: Key) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    }
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
            // The first pass after a change settles cached panel sizes; the
            // frames after it are the ones a user actually watches.
            let _ = paint_signature(&mut app, &ctx, input(width, 900.0));
            let first = paint_signature(&mut app, &ctx, input(width, 900.0));
            for _ in 0..3 {
                let next = paint_signature(&mut app, &ctx, input(width, 900.0));
                assert_eq!(
                    first,
                    next,
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

#[test]
fn review_hotkeys_reach_their_handlers() {
    use crate::testkit::TempRepo;

    let dir = TempDir::new("hotkeys");
    let db = Db::open_at(&dir.path().join("cra.db")).expect("open test db");
    let mut app = CraApp::with_db(db);
    app.settings.models.clear();

    let repo = TempRepo::new("hotkeys");
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
        .map(|(path, units)| crate::review::ReviewFile {
            path,
            units: units
                .into_iter()
                .map(crate::units::ReviewUnit::Comment)
                .collect(),
            edits: Vec::new(),
            decided: 0,
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
        branch_base: "main".into(),
        files,
        file_idx: 0,
        unit_idx: 0,
        decided_total: 0,
    });
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
    assert!(
        app.chosen.is_none(),
        "re-entering a comment starts from a clean slate"
    );

    // F focuses the follow-up box...
    review_frame(&mut app, &ctx, vec![key_press(Key::F)]);
    assert!(
        ctx.memory(|m| m.focused().is_some()),
        "F should put focus in the follow-up box"
    );

    // ...and once it has focus, letters are text, not commands. Without this
    // guard, typing "delete this line?" into the box would delete the comment,
    // skip to the next one, and keep the original along the way.
    review_frame(&mut app, &ctx, vec![key_press(Key::D)]);
    assert!(
        app.chosen.is_none(),
        "D was treated as a hotkey while typing"
    );
    assert_eq!(
        app.original_display, first,
        "the review moved on while typing"
    );
}
