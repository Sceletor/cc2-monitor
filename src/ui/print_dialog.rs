//! The print dialog: upload progress, file summary, Canvas slot mapping and
//! start options. Start is only possible while the printer is idle.

use egui::{Align2, Id, Layout, RichText, Sense, Vec2};
use serde_json::Value;

use super::theme;
use crate::app::App;
use crate::print_job::Stage;
use crate::printer::codes;

enum Action {
    Start,
    RetryDetails,
    Close,
}

pub fn draw(app: &mut App, ctx: &egui::Context) {
    if app.print_job.is_none() {
        return;
    }
    let idle = app.printer_idle();
    let busy = app
        .state
        .machine_status()
        .map(codes::machine_status_name)
        .unwrap_or("not connected");
    let mut action = None;

    egui::Window::new("Print")
        .id(Id::new("print_dialog"))
        .collapsible(false)
        .resizable(false)
        .default_width(540.0)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let Some(job) = app.print_job.as_mut() else { return };
            ui.set_min_width(520.0);

            // Summary: thumbnail, name, source, slicer estimates.
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(104.0), Sense::hover());
                ui.painter().rect_filled(rect, 6, theme::BG);
                match &job.thumb {
                    Some(tex) => theme::paint_fit(ui, rect.shrink(4.0), tex),
                    None => theme::placeholder_text(ui, rect, "No preview"),
                }
                ui.vertical(|ui| {
                    ui.add(egui::Label::new(RichText::new(&job.filename).size(16.0).strong()).truncate());
                    let source = match (&job.local_path, job.media) {
                        (Some(p), _) => format!("Sent from {}", p.display()),
                        (None, "udisk") => "On the USB drive".into(),
                        _ => "On the printer".into(),
                    };
                    ui.add(egui::Label::new(RichText::new(source).small().color(theme::MUTED)).truncate());
                    ui.add_space(4.0);
                    let n = |k: &str| job.detail.get(k).and_then(Value::as_f64).filter(|&v| v > 0.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 22.0;
                        theme::stat(ui, "Print time", n("print_time").map_or("—".into(), |t| theme::fmt_duration(t as i64)));
                        theme::stat(ui, "Filament", n("total_filament_used").map_or("—".into(), |g| format!("{g:.1} g")));
                        theme::stat(ui, "Layers", n("layer").map_or("—".into(), |l| format!("{l:.0}")));
                    });
                });
            });
            ui.separator();

            match &job.stage {
                Stage::Uploading { progress, .. } => {
                    let sent = progress.sent.load(std::sync::atomic::Ordering::Relaxed);
                    let total = progress.total.load(std::sync::atomic::Ordering::Relaxed);
                    ui.label("Uploading to the printer…");
                    ui.add(
                        egui::ProgressBar::new(progress.fraction())
                            .text(format!("{} of {}", theme::fmt_bytes(sent), theme::fmt_bytes(total)))
                            .fill(theme::ACCENT),
                    );
                    ui.add_space(6.0);
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel upload").clicked() {
                            action = Some(Action::Close);
                        }
                    });
                    return;
                }
                Stage::LoadingDetails { .. } => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading file details from the printer…");
                    });
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            action = Some(Action::Close);
                        }
                    });
                    return;
                }
                Stage::DetailsFailed(err) => {
                    ui.label(RichText::new(err).color(theme::ERR));
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Retry file details").clicked() {
                            action = Some(Action::RetryDetails);
                        }
                        if ui.button("Cancel").clicked() {
                            action = Some(Action::Close);
                        }
                    });
                    return;
                }
                Stage::SettingRefill { .. } | Stage::VerifyingRefill { .. } => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Verifying CANVAS auto-refill setting before starting…");
                    });
                    return;
                }
                Stage::Failed(err) => {
                    ui.label(RichText::new(err).color(theme::ERR));
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            action = Some(Action::Close);
                        }
                    });
                    return;
                }
                Stage::Ready | Stage::Starting { .. } => {}
            }
            let starting = matches!(job.stage, Stage::Starting { .. });

            ui.label(theme::section_title("Filament"));
            if job.canvas_connected {
                ui.checkbox(&mut job.use_external_spool, "Use external spool (disable CANVAS slot mapping)")
                    .on_hover_text("Requires deliberate selection. No CANVAS slot mapping is sent.");
            }
            if !job.canvas_status_verified {
                ui.label(RichText::new("CANVAS status unavailable; printing is blocked.").color(theme::ERR));
            } else if job.use_external_spool || !job.canvas_connected {
                ui.label(RichText::new("External spool selected; confirm the filament physically loaded.").color(theme::WARN));
            } else if job.trays.is_empty() {
                ui.label(RichText::new("CANVAS connected, but no loaded slots are available. Choose external spool explicitly or load filament.").color(theme::ERR));
            } else if job.tools.is_empty() {
                ui.label(
                    RichText::new("The file has no color information, so the printer chooses the slot.")
                        .color(theme::MUTED),
                );
            } else {
                egui::Grid::new("slot_map").num_columns(3).spacing([10.0, 6.0]).show(ui, |ui| {
                    for i in 0..job.tools.len() {
                        let tool = job.tools[i].clone();
                        ui.horizontal(|ui| {
                            theme::swatch(ui, theme::parse_hex_color(&tool.color), 18.0);
                            ui.label(format!("T{}  {}", tool.t + 1, tool.kind));
                        });
                        ui.label(RichText::new("»").color(theme::MUTED));
                        ui.horizontal(|ui| {
                            let selected = job.mapping[i].map(|m| &job.trays[m]);
                            theme::swatch(ui, selected.and_then(|t| theme::parse_hex_color(&t.color)), 18.0);
                            let text = selected.map_or("Choose a slot".into(), |t| t.label());
                            egui::ComboBox::from_id_salt(("slot", i))
                                .width(250.0)
                                .selected_text(text)
                                .show_ui(ui, |ui| {
                                    for (k, tray) in job.trays.iter().enumerate() {
                                        ui.horizontal(|ui| {
                                            theme::swatch(ui, theme::parse_hex_color(&tray.color), 14.0);
                                            ui.selectable_value(&mut job.mapping[i], Some(k), tray.label());
                                        });
                                    }
                                });
                        });
                        ui.end_row();
                        if let Some(warning) = job.mismatch(i) {
                            ui.label("");
                            ui.label("");
                            ui.label(RichText::new(format!("⚠ {warning}")).small().color(theme::WARN));
                            ui.end_row();
                        }
                    }
                });
            }

            if !job.use_external_spool && job.canvas_connected {
                let material_mismatch = job.tools.iter().enumerate().any(|(i, tool)| {
                    job.mapping.get(i).copied().flatten()
                        .and_then(|idx| job.trays.get(idx))
                        .is_some_and(|tray| tool.kind.trim().is_empty()
                            || !tool.kind.eq_ignore_ascii_case(&tray.kind))
                });
                if material_mismatch {
                    ui.checkbox(&mut job.allow_material_mismatch,
                        "I accept the material mismatch and its temperature risks")
                        .on_hover_text("Only enable when you deliberately want different material than the G-code expects.");
                } else {
                    job.allow_material_mismatch = false;
                }
                if !job.mapping_complete() {
                    ui.label(RichText::new("Printing disabled until every tool has a verified, compatible slot.").color(theme::WARN));
                }
            }
            ui.add_space(6.0);
            ui.label(theme::section_title("Options"));
            ui.horizontal(|ui| {
                ui.label("Plate side");
                ui.selectable_value(&mut job.smooth_plate, false, "Textured (A)")
                    .on_hover_text("High-temperature textured PEI side");
                ui.selectable_value(&mut job.smooth_plate, true, "Smooth (B)")
                    .on_hover_text("Low-temperature smooth side");
            });
            ui.checkbox(&mut job.level_bed, "Auto bed leveling before printing");
            ui.checkbox(&mut job.timelapse, "Record a timelapse");
            if job.canvas_connected && !job.use_external_spool {
                let changed = job.auto_refill != job.printer_auto_refill;
                ui.checkbox(&mut job.auto_refill, "Canvas auto refill")
                    .on_hover_text(
                        "When a spool runs out, continue from another slot with the same \
                         filament. This is a printer setting, so it stays as set after the print.",
                    );
                if changed {
                    ui.label(
                        RichText::new("Auto refill will be changed on the printer before starting.")
                            .small()
                            .color(theme::MUTED),
                    );
                }
            }

            if let Some(err) = &job.error {
                ui.add_space(4.0);
                ui.label(RichText::new(err).color(theme::ERR));
            }
            if !idle {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "The printer is busy ({busy}). Start becomes available once it is idle."
                    ))
                    .color(theme::WARN),
                );
            }

            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                let can_start = idle && !starting && job.mapping_complete();
                if ui.add_enabled(can_start, theme::accent_button("▶ Start print")).clicked() {
                    action = Some(Action::Start);
                }
                if ui.add_enabled(!starting, egui::Button::new("Cancel")).clicked() {
                    action = Some(Action::Close);
                }
                if starting {
                    ui.spinner();
                    ui.label("Starting…");
                }
            });
        });

    match action {
        Some(Action::Start) => app.start_print(),
        Some(Action::RetryDetails) => app.retry_job_details(),
        Some(Action::Close) => app.cancel_print_job(),
        None => {}
    }
}
