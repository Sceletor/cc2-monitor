//! Starting a print: the job being prepared and Canvas slot matching.
//!
//! Flow: (upload →) fetch file detail (1046) for its `color_map` → map each
//! G-code tool to a Canvas tray → send start (1020). The slot map must cover
//! every tool: per the protocol notes, a partial or out-of-range entry is
//! acknowledged with `error_code: 0` and the printer silently uses tray 0.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Instant;

use egui::TextureHandle;
use serde_json::{json, Value};

use crate::printer::upload::Progress;
use crate::ui::theme::parse_hex_color;

/// Above this RGB distance the dialog warns that colors differ.
pub const COLOR_WARN_DISTANCE: f32 = 120.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Tray {
    pub canvas_id: i64,
    pub tray_id: i64,
    pub kind: String,
    pub color: String,
    pub name: String,
}

impl Tray {
    pub fn label(&self) -> String {
        format!(
            "Canvas {} · Slot {} · {}",
            self.canvas_id + 1,
            self.tray_id + 1,
            if self.name.is_empty() { &self.kind } else { &self.name }
        )
    }
}

/// One G-code tool (T0, T1, …) and the filament the slicer assigned to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub t: i64,
    pub kind: String,
    pub color: String,
}

pub enum Stage {
    Uploading {
        progress: Arc<Progress>,
        done: Receiver<Result<(), String>>,
    },
    LoadingDetails {
        since: Instant,
    },
    Ready,
    DetailsFailed(String),
    SettingRefill { since: Instant, request_id: u64, expected: bool },
    VerifyingRefill { since: Instant, request_id: u64, expected: bool },
    Starting {
        since: Instant,
        request_id: u64,
    },
    Failed(String),
}

pub struct PrintJob {
    pub filename: String,
    /// `local` (printer storage) or `udisk`.
    pub media: &'static str,
    pub local_path: Option<PathBuf>,
    pub stage: Stage,
    pub detail: Value,
    pub tools: Vec<Tool>,
    pub trays: Vec<Tray>,
    /// For each tool, an index into `trays`.
    pub mapping: Vec<Option<usize>>,
    /// Request ID of the file-detail response we are waiting for.
    pub details_request_id: Option<u64>,
    pub detail_verified: bool,
    pub canvas_status_verified: bool,
    pub canvas_connected: bool,
    /// Explicit user overrides; never enabled by automatic slot assignment.
    pub use_external_spool: bool,
    pub allow_material_mismatch: bool,
    pub level_bed: bool,
    pub timelapse: bool,
    /// Plate side: false = textured high-temp (A), true = smooth low-temp (B).
    pub smooth_plate: bool,
    /// Canvas auto refill as the user wants it for this print.
    pub auto_refill: bool,
    /// Canvas auto refill as the printer last reported it.
    pub printer_auto_refill: bool,
    pub thumb: Option<TextureHandle>,
    pub error: Option<String>,
}

impl PrintJob {
    pub fn new(filename: String, media: &'static str, stage: Stage) -> Self {
        Self {
            filename,
            media,
            local_path: None,
            stage,
            detail: Value::Null,
            tools: Vec::new(),
            trays: Vec::new(),
            mapping: Vec::new(),
            details_request_id: None,
            detail_verified: false,
            canvas_status_verified: false,
            canvas_connected: false,
            use_external_spool: false,
            allow_material_mismatch: false,
            level_bed: true,
            timelapse: false,
            smooth_plate: false,
            auto_refill: false,
            printer_auto_refill: false,
            thumb: None,
            error: None,
        }
    }

    /// Fill in tools from a 1046 reply and choose initial tray assignments.
    pub fn set_detail(&mut self, detail: Value, canvas: &Value) {
        self.detail_verified = detail.get("color_map").and_then(Value::as_array).is_some();
        self.canvas_status_verified = canvas.get("canvas_list").and_then(Value::as_array).is_some();
        self.canvas_connected = canvas.get("canvas_list").and_then(Value::as_array).is_some_and(|units| {
            units.iter().any(|unit| unit.get("connected").and_then(Value::as_i64) == Some(1))
        });
        self.tools = tools_from_detail(&detail);
        self.trays = trays_from_canvas(canvas);
        self.mapping = auto_map(&self.tools, &self.trays);
        self.printer_auto_refill = canvas
            .get("auto_refill")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.auto_refill = self.printer_auto_refill;
        self.detail = detail;
        self.stage = Stage::Ready;
    }

    /// Fail closed if file metadata, Canvas state, mappings or material types are unverified.
    pub fn mapping_complete(&self) -> bool {
        if !self.detail_verified || !self.canvas_status_verified {
            return false;
        }
        if self.use_external_spool || !self.canvas_connected {
            return true;
        }
        !self.tools.is_empty()
            && self.tools.len() == self.mapping.len()
            && self.mapping.iter().enumerate().all(|(i, selection)| {
                selection
                    .and_then(|idx| self.trays.get(idx))
                    .is_some_and(|tray| {
                        self.allow_material_mismatch
                            || (!self.tools[i].kind.trim().is_empty()
                                && self.tools[i].kind.eq_ignore_ascii_case(&tray.kind))
                    })
            })
    }

    pub fn start_params(&self) -> Value {
        let slot_map: Vec<Value> = if self.use_external_spool || !self.canvas_connected {
            Vec::new()
        } else {
            self.tools
                .iter()
                .zip(&self.mapping)
                .filter_map(|(tool, m)| {
                    let tray = self.trays.get((*m)?)?;
                    Some(json!({"t": tool.t, "canvas_id": tray.canvas_id, "tray_id": tray.tray_id}))
                })
                .collect()
        };
        json!({
            "storage_media": self.media,
            "filename": self.filename,
            "config": {
                "delay_video": self.timelapse,
                "printer_check": self.level_bed,
                // As elegoo-link sends it: "A" textured plate, "B" smooth plate.
                "print_layout": if self.smooth_plate { "B" } else { "A" },
                "bedlevel_force": false,
                "slot_map": slot_map,
            }
        })
    }

    /// Warning for a tool's current assignment, if the filament looks wrong.
    pub fn mismatch(&self, tool_index: usize) -> Option<String> {
        let tool = &self.tools[tool_index];
        let tray = self.trays.get(self.mapping.get(tool_index).copied().flatten()?)?;
        if !tool.kind.is_empty() && !tool.kind.eq_ignore_ascii_case(&tray.kind) {
            return Some(format!("File expects {}, slot has {}", tool.kind, tray.kind));
        }
        match color_distance(&tool.color, &tray.color) {
            Some(d) if d > COLOR_WARN_DISTANCE => Some("Color differs from the file".into()),
            _ => None,
        }
    }
}

pub fn tools_from_detail(detail: &Value) -> Vec<Tool> {
    let Some(list) = detail.get("color_map").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut tools: Vec<Tool> = list
        .iter()
        .map(|c| Tool {
            t: c.get("t").and_then(Value::as_i64).unwrap_or(0),
            kind: c.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
            color: c.get("color").and_then(Value::as_str).unwrap_or_default().to_string(),
        })
        .collect();
    tools.sort_by_key(|t| t.t);
    tools.dedup_by_key(|t| t.t);
    tools
}

/// Loaded trays on connected Canvas units.
pub fn trays_from_canvas(canvas: &Value) -> Vec<Tray> {
    let units = canvas.get("canvas_list").and_then(Value::as_array);
    let mut out = Vec::new();
    for unit in units.into_iter().flatten() {
        if unit.get("connected").and_then(Value::as_i64) != Some(1) {
            continue;
        }
        let canvas_id = unit.get("canvas_id").and_then(Value::as_i64).unwrap_or(0);
        let trays = unit.get("tray_list").and_then(Value::as_array);
        for tray in trays.into_iter().flatten() {
            let s = |k: &str| tray.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            let empty = tray.get("status").and_then(Value::as_i64) == Some(0);
            if empty || s("filament_type").is_empty() {
                continue;
            }
            out.push(Tray {
                canvas_id,
                tray_id: tray.get("tray_id").and_then(Value::as_i64).unwrap_or(0),
                kind: s("filament_type"),
                color: s("filament_color"),
                name: s("filament_name"),
            });
        }
    }
    out
}

/// Pick a tray for each tool: same material first, then nearest color,
/// preferring trays no other tool has taken.
pub fn auto_map(tools: &[Tool], trays: &[Tray]) -> Vec<Option<usize>> {
    let mut used = vec![false; trays.len()];
    tools
        .iter()
        .map(|tool| {
            // Material compatibility is mandatory. Never prefer a free PETG tray
            // over reusing an already-selected PLA tray, for example.
            let score = |i: usize| {
                let reuse_penalty = if used[i] { 1000.0 } else { 0.0 };
                reuse_penalty + color_distance(&tool.color, &trays[i].color).unwrap_or(500.0)
            };
            let best = (0..trays.len())
                .filter(|&i| !tool.kind.trim().is_empty() && tool.kind.eq_ignore_ascii_case(&trays[i].kind))
                .min_by(|&a, &b| score(a).total_cmp(&score(b)))?;
            used[best] = true;
            Some(best)
        })
        .collect()
}

pub fn color_distance(a: &str, b: &str) -> Option<f32> {
    let (a, b) = (parse_hex_color(a)?, parse_hex_color(b)?);
    let d = |x: u8, y: u8| (x as f32 - y as f32).powi(2);
    Some((d(a.r(), b.r()) + d(a.g(), b.g()) + d(a.b(), b.b())).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas() -> Value {
        json!({"canvas_list": [{"canvas_id": 0, "connected": 1, "tray_list": [
            {"tray_id": 0, "filament_type": "PLA", "filament_color": "#9B30FF", "filament_name": "PLA", "status": 1},
            {"tray_id": 1, "filament_type": "", "filament_color": "", "status": 0},
            {"tray_id": 2, "filament_type": "PLA", "filament_color": "#000000", "filament_name": "PLA", "status": 1},
            {"tray_id": 3, "filament_type": "PETG", "filament_color": "#FFFF00", "filament_name": "PETG", "status": 1},
        ]}]})
    }

    #[test]
    fn empty_trays_are_skipped() {
        let trays = trays_from_canvas(&canvas());
        assert_eq!(trays.iter().map(|t| t.tray_id).collect::<Vec<_>>(), [0, 2, 3]);
    }

    #[test]
    fn maps_by_material_then_color_without_reuse() {
        let trays = trays_from_canvas(&canvas());
        let tools = tools_from_detail(&json!({"color_map": [
            {"t": 1, "name": "PETG", "color": "#FFEE00"},
            {"t": 0, "name": "PLA", "color": "#111111"},
            {"t": 2, "name": "PLA", "color": "#101010"},
        ]}));
        assert_eq!(tools.iter().map(|t| t.t).collect::<Vec<_>>(), [0, 1, 2]);
        let m = auto_map(&tools, &trays);
        // T0 black PLA → tray 2, T1 PETG → tray 3, T2 black PLA again → the other PLA (tray 0).
        let picked: Vec<i64> = m.iter().map(|i| trays[i.unwrap()].tray_id).collect();
        assert_eq!(picked, [2, 3, 0]);
    }

    #[test]
    fn start_params_cover_every_tool() {
        let mut job = PrintJob::new("chute.gcode".into(), "local", Stage::Ready);
        job.set_detail(
            json!({"color_map": [{"t": 0, "name": "PLA", "color": "#898989"}]}),
            &canvas(),
        );
        let p = job.start_params();
        assert_eq!(p["filename"], "chute.gcode");
        assert_eq!(p["config"]["printer_check"], true);
        assert_eq!(p["config"]["delay_video"], false);
        let map = p["config"]["slot_map"].as_array().unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map[0]["t"], 0);
        assert!(map[0].get("canvas_id").is_some() && map[0].get("tray_id").is_some());
    }

    #[test]
    fn plate_side_and_auto_refill() {
        let mut c = canvas();
        c["auto_refill"] = json!(true);
        let mut job = PrintJob::new("a.gcode".into(), "local", Stage::Ready);
        job.set_detail(json!({"color_map": []}), &c);
        assert!(job.auto_refill && job.printer_auto_refill);
        assert_eq!(job.start_params()["config"]["print_layout"], "A");
        job.smooth_plate = true;
        assert_eq!(job.start_params()["config"]["print_layout"], "B");
    }

    #[test]
    fn no_canvas_sends_empty_slot_map() {
        let mut job = PrintJob::new("a.gcode".into(), "local", Stage::Ready);
        job.set_detail(json!({"color_map": [{"t": 0, "name": "PLA", "color": "#fff"}]}), &json!({"canvas_list": []}));
        assert!(job.mapping_complete());
        assert_eq!(job.start_params()["config"]["slot_map"], json!([]));
    }

    #[test]
    fn mismatch_warns_on_material() {
        let mut job = PrintJob::new("a.gcode".into(), "local", Stage::Ready);
        job.set_detail(json!({"color_map": [{"t": 0, "name": "ABS", "color": "#000000"}]}), &canvas());
        assert!(!job.mapping_complete());
        // An incompatible tray must be selected deliberately; then explicit
        // acceptance is required before enabling the Start button.
        job.mapping[0] = Some(2);
        assert!(job.mismatch(0).unwrap().contains("ABS"));
        assert!(!job.mapping_complete());
        job.allow_material_mismatch = true;
        assert!(job.mapping_complete());
    }
    #[test]
    fn never_prefers_wrong_material_over_reusing_a_tray() {
        let trays = trays_from_canvas(&canvas());
        let tools = vec![
            Tool { t: 0, kind: "PLA".into(), color: "#111111".into() },
            Tool { t: 1, kind: "PLA".into(), color: "#111111".into() },
            Tool { t: 2, kind: "PLA".into(), color: "#111111".into() },
        ];
        let picks = auto_map(&tools, &trays);
        assert!(picks.iter().all(|idx| matches!(idx, Some(0) | Some(1))));
        assert!(picks.iter().all(|idx| trays[idx.unwrap()].kind == "PLA"));
    }

    #[test]
    fn missing_metadata_or_canvas_status_blocks_start() {
        let mut job = PrintJob::new("a.gcode".into(), "local", Stage::Ready);
        job.set_detail(Value::Null, &canvas());
        assert!(!job.mapping_complete());
        job.set_detail(json!({"color_map": [{"t": 0, "name": "PLA"}]}), &Value::Null);
        assert!(!job.mapping_complete());
    }

    #[test]
    fn empty_canvas_slots_do_not_silently_select_external_spool() {
        let mut job = PrintJob::new("a.gcode".into(), "local", Stage::Ready);
        let c = json!({"canvas_list": [{ "connected": 1, "canvas_id": 0, "tray_list": [] }]});
        job.set_detail(json!({"color_map": [{"t": 0, "name": "PLA"}]}), &c);
        assert!(!job.mapping_complete());
        job.use_external_spool = true;
        assert!(job.mapping_complete());
        assert_eq!(job.start_params()["config"]["slot_map"], json!([]));
    }

}
