#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod comments;
mod db;
mod diffparse;
mod gitio;
mod models;
mod review;
mod settings;
mod ui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1480.0, 940.0])
            .with_min_inner_size([1100.0, 700.0])
            .with_title("Code Review Assistant — comments"),
        ..Default::default()
    };
    eframe::run_native(
        "code-review-assistant",
        options,
        Box::new(|cc| Ok(Box::new(app::CraApp::new(cc)))),
    )
}
