// No console window behind the GUI in release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ai;
mod app;
mod camera;
mod config;
mod print_job;
mod printer;
mod secret;
mod ui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CC2 Monitor")
            .with_app_id("cc2-monitor")
            .with_inner_size([1440.0, 920.0])
            .with_min_inner_size([1040.0, 680.0])
            .with_icon(ui::theme::app_icon())
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "CC2 Monitor",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
