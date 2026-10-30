//! Right-hand column: temperatures, fans, speed/light, filament, motion, device.

use std::time::Instant;

use egui::{Color32, RichText, Sense, Stroke, StrokeKind, Vec2};
use serde_json::{json, Value};

use super::theme;
use crate::app::{App, Confirm, Pending};
use crate::printer::codes;

const NOZZLE_MAX: f64 = 350.0;
const BED_MAX: f64 = 120.0;

/// (status key, label) for fans the user can set.
const CONTROL_FANS: [(&str, &str); 3] = [
    ("fan", "Part cooling"),
    ("aux_fan", "Auxiliary"),
    ("box_fan", "Chamber"),
];
const READONLY_FANS: [(&str, &str); 2] = [("heater_fan", "Heatbreak"), ("controller_fan", "Controller")];

/// (name, nozzle °C, bed °C)
const PRESETS: [(&str, f64, f64); 4] = [
    ("PLA", 210.0, 60.0),
    ("PETG", 240.0, 80.0),
    ("ABS", 260.0, 100.0),
    ("Cool down", 0.0, 0.0),
];

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    ui.spacing_mut().item_spacing.y = 10.0;
    temperatures(app, ui);
    fans(app, ui);
    speed_and_light(app, ui);
    filament(app, ui);
    motion(app, ui);
    device(app, ui);
}

fn temperatures(app: &mut App, ui: &mut egui::Ui) {
    let ready = app.is_ready();
    let mut set_nozzle = None;
    let mut set_bed = None;

    theme::card(ui, "Temperatures", |ui| {
        egui::Grid::new("temps")
            .num_columns(3)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                set_nozzle = heater_row(
                    ui,
                    "Nozzle",
                    theme::NOZZLE,
                    app.state.f("extruder.temperature"),
                    app.state.f("extruder.target"),
                    &mut app.ui.nozzle_input,
                    NOZZLE_MAX,
                    ready,
                );
                set_bed = heater_row(
                    ui,
                    "Bed",
                    theme::BED,
                    app.state.f("heater_bed.temperature"),
                    app.state.f("heater_bed.target"),
                    &mut app.ui.bed_input,
                    BED_MAX,
                    ready,
                );

                dot_label(ui, "Chamber", theme::CHAMBER);
                ui.label(
                    RichText::new(theme::fmt_temp(app.state.f("ztemperature_sensor.temperature")))
                        .size(16.0)
                        .strong(),
                );
                ui.end_row();
            });

        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            for (name, nozzle, bed) in PRESETS {
                let hint = if nozzle > 0.0 {
                    format!("Nozzle {nozzle:.0}° / bed {bed:.0}°")
                } else {
                    "Turn both heaters off".into()
                };
                if ui
                    .add_enabled(ready, egui::Button::new(name).small())
                    .on_hover_text(hint)
                    .clicked()
                {
                    set_nozzle = Some(nozzle);
                    set_bed = Some(bed);
                }
            }
        });
    });

    let params = match (set_nozzle, set_bed) {
        (Some(n), Some(b)) => json!({"extruder": n, "heater_bed": b}),
        (Some(n), None) => json!({"extruder": n}),
        (None, Some(b)) => json!({"heater_bed": b}),
        (None, None) => return,
    };
    app.send(1028, params);
}

/// One heater line in the grid. Returns a new target when the user sets one.
#[allow(clippy::too_many_arguments)]
fn heater_row(
    ui: &mut egui::Ui,
    name: &str,
    color: Color32,
    current: Option<f64>,
    target: Option<f64>,
    input: &mut String,
    max: f64,
    ready: bool,
) -> Option<f64> {
    dot_label(ui, name, color);
    ui.horizontal(|ui| {
        ui.label(RichText::new(theme::fmt_temp(current)).size(16.0).strong());
        let target_text = match target {
            Some(t) if t > 0.0 => format!("/ {t:.0}°"),
            _ => "/ off".into(),
        };
        ui.label(RichText::new(target_text).color(theme::MUTED));
    });

    let mut out = None;
    ui.horizontal(|ui| {
        let edit = ui.add_enabled(
            ready,
            egui::TextEdit::singleline(input)
                .desired_width(46.0)
                .hint_text("°C"),
        );
        let entered = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.add_enabled(ready, egui::Button::new("Set")).clicked() || entered {
            if let Ok(v) = input.trim().parse::<f64>() {
                out = Some(v.clamp(0.0, max));
                input.clear();
            }
        }
        if ui.add_enabled(ready, egui::Button::new("Off")).clicked() {
            out = Some(0.0);
        }
    });
    ui.end_row();
    out
}

fn dot_label(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.5, color);
        ui.label(text);
    });
}

fn fans(app: &mut App, ui: &mut egui::Ui) {
    let ready = app.is_ready();
    let mut send = Vec::new();

    theme::card(ui, "Fans", |ui| {
        egui::Grid::new("fans")
            .num_columns(3)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                for (i, (key, label)) in CONTROL_FANS.iter().enumerate() {
                    let actual = app.state.fan_pct(key).unwrap_or(0.0);
                    let pending = &mut app.ui.fan_pending[i];
                    // Drop the override once the printer reports the new value.
                    if pending.is_some_and(|p| (p.value - actual).abs() < 2.0) {
                        *pending = None;
                    }
                    let mut value = pending.map_or(actual, |p| p.value);

                    ui.label(*label);
                    ui.spacing_mut().slider_width = 150.0;
                    let resp = ui.add_enabled(
                        ready,
                        egui::Slider::new(&mut value, 0.0..=100.0)
                            .suffix("%")
                            .integer(),
                    );
                    if resp.changed() {
                        *pending = Some(Pending {
                            value,
                            at: Instant::now(),
                        });
                    }
                    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                        let pwm = (value / 100.0 * 255.0).round() as i64;
                        send.push(json!({ *key: pwm }));
                    }
                    rpm_label(ui, app.state.fan_rpm(key));
                    ui.end_row();
                }
                for (key, label) in READONLY_FANS {
                    ui.label(RichText::new(label).color(theme::MUTED));
                    ui.label(
                        app.state
                            .fan_pct(key)
                            .map_or("—".into(), |p| format!("{p:.0}%")),
                    );
                    rpm_label(ui, app.state.fan_rpm(key));
                    ui.end_row();
                }
            });
    });

    for params in send {
        app.send(1030, params);
    }
}

fn rpm_label(ui: &mut egui::Ui, rpm: Option<i64>) {
    let text = rpm.map_or(String::new(), |r| format!("{r} rpm"));
    ui.label(RichText::new(text).small().color(theme::MUTED));
}

fn speed_and_light(app: &mut App, ui: &mut egui::Ui) {
    let ready = app.is_ready();
    let mode = app.state.i("gcode_move.speed_mode");
    let led_on = app.state.i("led.status") == Some(1);

    theme::card(ui, "Speed & light", |ui| {
        ui.horizontal(|ui| {
            for (m, name) in codes::SPEED_MODES {
                let selected = mode == Some(m);
                if ui
                    .add_enabled(ready, egui::Button::new(name).selected(selected))
                    .clicked()
                    && !selected
                {
                    app.send(1031, json!({"mode": m}));
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Chamber light");
            let label = if led_on { "💡 On" } else { "Off" };
            if ui
                .add_enabled(ready, egui::Button::new(label).selected(led_on))
                .clicked()
            {
                app.send(1029, json!({"power": if led_on { 0 } else { 1 }}));
            }
            if let Some(speed) = app.state.f("gcode_move.speed").filter(|_| app.state.is_printing()) {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{:.0} mm/s", speed / 60.0)).color(theme::MUTED),
                    );
                });
            }
        });
    });
}

fn filament(app: &mut App, ui: &mut egui::Ui) {
    let active_unit = app.state.canvas.get("active_canvas_id").and_then(Value::as_i64);
    let active_tray = app.state.canvas.get("active_tray_id").and_then(Value::as_i64);

    theme::card(ui, "Filament", |ui| {
        let units: Vec<&Value> = app
            .state
            .canvas_units()
            .into_iter()
            .filter(|u| u.get("connected").and_then(Value::as_i64) == Some(1))
            .collect();

        if units.is_empty() {
            ui.label(RichText::new("No Canvas connected").color(theme::MUTED));
        }
        for unit in units {
            let id = unit.get("canvas_id").and_then(Value::as_i64).unwrap_or(0);
            ui.label(RichText::new(format!("Canvas {}", id + 1)).small().color(theme::MUTED));
            ui.horizontal(|ui| {
                let trays = unit.get("tray_list").and_then(Value::as_array);
                for tray in trays.into_iter().flatten() {
                    let tid = tray.get("tray_id").and_then(Value::as_i64).unwrap_or(0);
                    let active = active_unit == Some(id) && active_tray == Some(tid);
                    spool(ui, tray, tid, active);
                }
            });
        }

        let detected = app.state.i("extruder.filament_detected");
        let sensor = match detected {
            Some(1) => ("Filament sensor: loaded", theme::OK),
            Some(0) => ("Filament sensor: empty", theme::WARN),
            _ => ("Filament sensor: —", theme::MUTED),
        };
        ui.label(RichText::new(sensor.0).small().color(sensor.1));
    });
}

fn spool(ui: &mut egui::Ui, tray: &Value, tray_id: i64, active: bool) {
    let s = |k: &str| tray.get(k).and_then(Value::as_str).unwrap_or_default();
    let empty = tray.get("status").and_then(Value::as_i64) == Some(0) || s("filament_type").is_empty();
    let color = theme::parse_hex_color(s("filament_color")).filter(|_| !empty);

    let (rect, resp) = ui.allocate_exact_size(Vec2::new(70.0, 74.0), Sense::hover());
    let painter = ui.painter();
    let swatch = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width(), 46.0));
    match color {
        Some(c) => {
            painter.rect_filled(swatch, 6, c);
            painter.text(
                swatch.center(),
                egui::Align2::CENTER_CENTER,
                (tray_id + 1).to_string(),
                egui::FontId::proportional(15.0),
                theme::contrast_text(c),
            );
        }
        None => {
            painter.rect_stroke(swatch, 6, Stroke::new(1.0_f32, theme::CARD_STROKE), StrokeKind::Inside);
            painter.text(
                swatch.center(),
                egui::Align2::CENTER_CENTER,
                "empty",
                egui::FontId::proportional(11.0),
                theme::MUTED,
            );
        }
    }
    if active {
        painter.rect_stroke(swatch.expand(2.0), 7, Stroke::new(2.0_f32, theme::ACCENT), StrokeKind::Outside);
    }
    let label = if empty { "—" } else { s("filament_type") };
    painter.text(
        egui::pos2(rect.center().x, swatch.bottom() + 14.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(12.0),
        if active { theme::ACCENT } else { theme::TEXT },
    );

    if !empty {
        let min = tray.get("min_nozzle_temp").and_then(Value::as_i64).unwrap_or(0);
        let max = tray.get("max_nozzle_temp").and_then(Value::as_i64).unwrap_or(0);
        resp.on_hover_text(format!(
            "{} {}\n{}\nNozzle {min}–{max} °C{}",
            s("brand"),
            s("filament_name"),
            s("filament_color"),
            if active { "\nIn use" } else { "" }
        ));
    }
}

fn motion(app: &mut App, ui: &mut egui::Ui) {
    let ready = app.is_ready();
    // Only allow jogging/homing when the machine explicitly reports Idle.
    // Paused, calibrating, heating, loading filament, etc. are not safe states.
    let can_move = ready && app.state.machine_status() == Some(1);
    let homed = app
        .state
        .s("toolhead.homed_axes")
        .or(app.state.s("tool_head.homed_axes"))
        .unwrap_or("")
        .to_string();

    theme::card(ui, "Motion", |ui| {
        ui.horizontal(|ui| {
            for axis in ["x", "y", "z"] {
                let v = app.state.f(&format!("gcode_move.{axis}"));
                let color = if homed.contains(axis) { theme::TEXT } else { theme::MUTED };
                ui.label(RichText::new(axis.to_uppercase()).color(theme::MUTED));
                ui.label(
                    RichText::new(v.map_or("—".into(), |v| format!("{v:.2}")))
                        .monospace()
                        .color(color),
                );
                ui.add_space(6.0);
            }
        });
        let all_homed = homed.contains('x') && homed.contains('y') && homed.contains('z');
        let can_jog = can_move && all_homed;
        if app.state.has_status() && !all_homed {
            ui.label(RichText::new("Not all axes homed").small().color(theme::WARN));
        }

        ui.horizontal(|ui| {
            ui.label("Step");
            for step in [0.1, 1.0, 10.0, 50.0] {
                ui.selectable_value(&mut app.ui.jog_step, step, format!("{step} mm"));
            }
        });

        let step = app.ui.jog_step;
        let mut jog: Option<(&str, f64)> = None;
        let mut home = false;
        let btn = |ui: &mut egui::Ui, text: &str| {
            ui.add_enabled(can_jog, egui::Button::new(text).min_size(Vec2::new(44.0, 30.0)))
                .clicked()
        };
        ui.horizontal(|ui| {
            egui::Grid::new("jog_xy").spacing([4.0, 4.0]).show(ui, |ui| {
                ui.label("");
                if btn(ui, "Y+") {
                    jog = Some(("y", step));
                }
                ui.label("");
                ui.end_row();
                if btn(ui, "X−") {
                    jog = Some(("x", -step));
                }
                if ui.add_enabled(can_move, egui::Button::new("Home").min_size(Vec2::new(44.0, 30.0))).clicked() {
                    home = true;
                }
                if btn(ui, "X+") {
                    jog = Some(("x", step));
                }
                ui.end_row();
                ui.label("");
                if btn(ui, "Y−") {
                    jog = Some(("y", -step));
                }
                ui.end_row();
            });
            ui.add_space(16.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                if btn(ui, "Z+") {
                    jog = Some(("z", step));
                }
                if btn(ui, "Z−") {
                    jog = Some(("z", -step));
                }
            });
            ui.add_space(16.0);
            ui.vertical(|ui| {
                if ui
                    .add_enabled(ready, theme::danger_button("E-Stop"))
                    .on_hover_text("Emergency stop: halts the printer immediately")
                    .clicked()
                {
                    app.ui.confirm = Some(Confirm::EmergencyStop);
                }
            });
        });
        if ready && !can_move {
            ui.label(RichText::new("Movement is disabled unless the machine is idle").small().color(theme::MUTED));
        }

        if let Some((axis, distance)) = jog {
            app.send(1027, json!({"axes": axis, "distance": distance}));
        }
        if home {
            app.send(1026, json!({"homed_axes": "xyz"}));
        }
    });
}

fn device(app: &mut App, ui: &mut egui::Ui) {
    theme::card(ui, "Device", |ui| {
        let a = &app.state;
        let rows = [
            ("Model", a.attr("machine_model").map(str::to_string)),
            ("Name", a.attr("hostname").map(str::to_string)),
            ("Serial", Some(app.settings.serial.clone()).filter(|s| !s.is_empty())),
            ("Address", Some(app.settings.ip.clone()).filter(|s| !s.is_empty())),
            ("Firmware", a.attr("software_version.ota_version").map(str::to_string)),
            ("MCU", a.attr("software_version.mcu_version").map(str::to_string)),
            ("Hardware", a.attr("hardware_version").map(str::to_string)),
            ("USB drive", flag(a.i("external_device.u_disk").or(bool_i(a, "external_device.u_disk")))),
            ("Camera", flag(a.i("external_device.camera").or(bool_i(a, "external_device.camera")))),
        ];
        egui::Grid::new("device").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            for (k, v) in rows {
                ui.label(RichText::new(k).color(theme::MUTED));
                ui.label(v.unwrap_or_else(|| "—".into()));
                ui.end_row();
            }
        });
    });
}

fn bool_i(state: &crate::printer::PrinterState, path: &str) -> Option<i64> {
    crate::printer::state::get(&state.status, path)
        .and_then(Value::as_bool)
        .map(i64::from)
}

fn flag(v: Option<i64>) -> Option<String> {
    v.map(|v| if v != 0 { "Connected".into() } else { "Not connected".into() })
}
