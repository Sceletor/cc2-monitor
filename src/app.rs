//! Application state and the glue between the printer/camera workers and the UI.
//! Panel drawing lives in `crate::ui`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use egui::TextureHandle;
use serde_json::{json, Value};

use crate::ai;
use crate::camera::CameraStream;
use crate::config::{Settings, STORAGE_KEY};
use crate::print_job::{PrintJob, Stage};
use crate::printer::discovery::Discovered;
use crate::printer::download;
use crate::printer::upload::{self, Progress};
use crate::printer::{codes, ConnectConfig, Event, LogEntry, Phase, PrinterClient, PrinterState};
use crate::ui;

const LOG_CAPACITY: usize = 1000;
const TOAST_TTL: Duration = Duration::from_secs(6);
const DETAIL_TIMEOUT: Duration = Duration::from_secs(6);
const REFILL_TIMEOUT: Duration = Duration::from_secs(8);
const START_TIMEOUT: Duration = Duration::from_secs(15);
const RENDER_TIMEOUT: Duration = Duration::from_secs(300);
const RENDER_POLL: Duration = Duration::from_secs(5);
/// Sub-status the printer reports when a render finds no raw frames.
const SUB_TIMELAPSE_NO_FRAMES: i64 = 3022;

/// A timelapse MP4 being downloaded, keyed by history task id.
pub struct Transfer {
    pub progress: Arc<Progress>,
    done: Receiver<Result<(), String>>,
    dest: PathBuf,
}

/// A timelapse being rendered from raw frames on the printer (method 1051).
pub struct RenderJob {
    pub task_id: String,
    since: Instant,
    last_poll: Instant,
    start_sub_status: i64,
}

/// Where a requested thumbnail (method 1045) should go.
#[derive(Clone, Copy)]
enum ThumbFor {
    CurrentJob,
    PrintDialog,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Temperatures,
    History,
    Files,
    Log,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    StopPrint,
    EmergencyStop,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub at: Instant,
}

/// A value the user just set, shown instead of the printer's value until the
/// printer catches up (so sliders do not snap back while a command is in flight).
#[derive(Clone, Copy)]
pub struct Pending {
    pub value: f32,
    pub at: Instant,
}

pub struct UiState {
    pub tab: Tab,
    pub show_connect: bool,
    pub camera_expanded: bool,
    pub nozzle_input: String,
    pub bed_input: String,
    pub jog_step: f64,
    pub confirm: Option<Confirm>,
    pub scan: Option<Receiver<std::io::Result<Vec<Discovered>>>>,
    pub scan_results: Vec<Discovered>,
    pub scan_error: Option<String>,
    pub show_code: bool,
    pub fan_pending: [Option<Pending>; 3],
    pub log_filter: String,
    pub log_hide_status: bool,
    pub log_paused: bool,
    pub files_media: &'static str,
    pub chart_minutes: f64,
}

pub struct App {
    ctx: egui::Context,
    pub logo: TextureHandle,
    pub settings: Settings,
    pub client: Option<PrinterClient>,
    pub phase: Option<Phase>,
    pub state: PrinterState,
    pub camera: Option<CameraStream>,
    pub cam_tex: Option<TextureHandle>,
    pub last_jpeg: Option<Vec<u8>>,
    pub thumb: Option<TextureHandle>,
    thumb_for: Option<String>,
    pub log: VecDeque<LogEntry>,
    pub toasts: Vec<Toast>,
    pub ui: UiState,
    pub print_job: Option<PrintJob>,
    pub downloads: HashMap<String, Transfer>,
    /// Where each finished timelapse download was saved, by task id.
    pub saved: HashMap<String, PathBuf>,
    pub render: Option<RenderJob>,
    save_pick: Option<(String, String, Receiver<Option<PathBuf>>)>,
    file_pick: Option<Receiver<Option<PathBuf>>>,
    thumb_requests: HashMap<u64, ThumbFor>,
    last_sample: Instant,
    last_sub_status: i64,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        ui::theme::apply(&cc.egui_ctx);

        let mut settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, STORAGE_KEY))
            .unwrap_or_default();
        // Ignore legacy plaintext settings; restore only the DPAPI-protected code.
        if !settings.protected_access_code.is_empty() {
            if let Some(code) = crate::secret::unprotect(&settings.protected_access_code) {
                settings.access_code = code;
            } else {
                settings.access_code.clear();
                settings.auto_connect = false;
            }
        }
        // Point at a printer without going through the dialog (handy for dev).
        if let Ok(ip) = std::env::var("CC2_IP") {
            if ip.trim() != settings.ip {
                settings.serial.clear();
            }
            settings.ip = ip.trim().to_string();
            settings.auto_connect = true;
        }

        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            logo: ui::theme::logo_texture(&cc.egui_ctx),
            ui: UiState {
                tab: Tab::Temperatures,
                show_connect: settings.ip.trim().is_empty(),
                camera_expanded: false,
                nozzle_input: String::new(),
                bed_input: String::new(),
                jog_step: 10.0,
                confirm: None,
                scan: None,
                scan_results: Vec::new(),
                scan_error: None,
                show_code: false,
                fan_pending: [None; 3],
                log_filter: String::new(),
                log_hide_status: true,
                log_paused: false,
                files_media: "local",
                chart_minutes: 15.0,
            },
            settings,
            client: None,
            phase: None,
            state: PrinterState::default(),
            camera: None,
            cam_tex: None,
            last_jpeg: None,
            thumb: None,
            thumb_for: None,
            log: VecDeque::new(),
            toasts: Vec::new(),
            print_job: None,
            downloads: HashMap::new(),
            saved: HashMap::new(),
            render: None,
            save_pick: None,
            file_pick: None,
            thumb_requests: HashMap::new(),
            last_sample: Instant::now(),
            last_sub_status: 0,
        };

        if app.settings.auto_connect && !app.settings.ip.trim().is_empty() {
            app.connect(&cc.egui_ctx);
        }
        app
    }

    // ---- connection -----------------------------------------------------

    pub fn connect(&mut self, ctx: &egui::Context) {
        self.disconnect();
        let ip = self.settings.ip.trim().to_string();
        if ip.is_empty() {
            self.ui.show_connect = true;
            return;
        }
        if ip.parse::<std::net::Ipv4Addr>().is_err() {
            self.toast(ToastKind::Error, "Enter a valid printer IPv4 address, not a URL or hostname.");
            self.ui.show_connect = true;
            return;
        }
        let access_code = match self.settings.access_code.trim() {
            "" => crate::config::DEFAULT_ACCESS_CODE.to_string(),
            code => code.to_string(),
        };
        self.client = Some(PrinterClient::connect(
            ConnectConfig {
                ip,
                access_code,
                serial: self.settings.serial.clone(),
            },
            ctx.clone(),
        ));
        self.phase = Some(Phase::Connecting);
        if self.settings.camera_on {
            self.start_camera(ctx);
        }
    }

    pub fn disconnect(&mut self) {
        self.client = None;
        self.phase = None;
        self.stop_camera();
        self.state.clear();
        self.thumb = None;
        self.thumb_for = None;
        self.thumb_requests.clear();
        self.last_sub_status = 0;
        self.print_job = None;
    }

    pub fn is_ready(&self) -> bool {
        self.client.as_ref().is_some_and(PrinterClient::is_ready)
    }

    pub fn start_camera(&mut self, ctx: &egui::Context) {
        if self.settings.ip.trim().is_empty() {
            return;
        }
        self.camera = Some(CameraStream::start(
            &self.settings.ip,
            ctx.clone(),
            Box::new(ai::Disabled),
            self.settings.camera_fps,
            self.settings.camera_scale,
        ));
    }

    /// Push the frame rate / resolution settings to the running stream.
    pub fn apply_camera_limits(&self) {
        if let Some(cam) = &self.camera {
            cam.set_limits(self.settings.camera_fps, self.settings.camera_scale);
        }
    }

    pub fn stop_camera(&mut self) {
        self.camera = None;
        self.cam_tex = None;
        self.last_jpeg = None;
    }

    pub fn send(&mut self, method: i64, params: Value) -> Option<u64> {
        self.client.as_ref()?.send(method, params)
    }

    /// True when a new print may be started (machine status Idle).
    pub fn printer_idle(&self) -> bool {
        self.is_ready() && self.state.machine_status() == Some(1)
    }

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toasts.push(Toast {
            text: text.into(),
            kind,
            at: Instant::now(),
        });
    }

    // ---- incoming -------------------------------------------------------

    fn process_events(&mut self) {
        let Some(client) = &self.client else { return };
        let events: Vec<Event> = client.poll().collect();
        for ev in events {
            match ev {
                Event::Phase(p) => {
                    if p == Phase::Connected && self.phase != Some(Phase::Connected) {
                        self.toast(ToastKind::Success, "Connected to printer");
                    }
                    self.phase = Some(p);
                }
                Event::SnLearned(sn) => self.settings.serial = sn,
                Event::Status(msg) => {
                    if self.state.apply_delta(&msg) {
                        self.send(1002, json!({}));
                    }
                    self.after_status_change();
                }
                Event::Response { method, msg } => self.on_response(method, msg),
                Event::Log(entry) => {
                    if !self.ui.log_paused {
                        if self.log.len() >= LOG_CAPACITY {
                            self.log.pop_front();
                        }
                        self.log.push_back(entry);
                    }
                }
            }
        }
    }

    fn on_response(&mut self, method: i64, msg: Value) {
        let result = msg.get("result").cloned().unwrap_or(Value::Null);
        let error_code = result.get("error_code").and_then(Value::as_i64).unwrap_or(0);
        let id = msg.get("id").and_then(Value::as_u64);

        match method {
            1001 => self.state.attributes = result,
            1002 => {
                self.state.set_full_status(result);
                self.after_status_change();
            }
            2005 => {
                if let Some(info) = result.get("canvas_info") {
                    self.state.canvas = info.clone();
                }
                let expected = self.print_job.as_ref().and_then(|job| match job.stage {
                    Stage::VerifyingRefill { request_id, expected, .. } if id == Some(request_id) => Some(expected),
                    _ => None,
                });
                if let Some(expected) = expected {
                    if result.get("canvas_info").and_then(|v| v.get("auto_refill")).and_then(Value::as_bool) == Some(expected) {
                        if let Some(job) = &mut self.print_job {
                            job.printer_auto_refill = expected;
                        }
                        self.send_start_command();
                    } else {
                        self.refill_error("CANVAS did not confirm the requested auto-refill setting.");
                    }
                }
            }
            2004 => {
                let expected = self.print_job.as_ref().and_then(|job| match job.stage {
                    Stage::SettingRefill { request_id, expected, .. } if id == Some(request_id) => Some(expected),
                    _ => None,
                });
                if let Some(expected) = expected {
                    if result.get("error_code").and_then(Value::as_i64) != Some(0) {
                        self.refill_error("Printer rejected the CANVAS auto-refill change.");
                    } else if let Some(request_id) = self.send(2005, json!({})) {
                        if let Some(job) = &mut self.print_job {
                            job.stage = Stage::VerifyingRefill { since: Instant::now(), request_id, expected };
                        }
                    } else {
                        self.refill_error("Could not verify the CANVAS auto-refill change.");
                    }
                }
            }
            1036 => {
                if let Some(list) = result.get("history_task_list").and_then(Value::as_array) {
                    let mut list = list.clone();
                    list.sort_by_key(|e| {
                        std::cmp::Reverse(e.get("begin_time").and_then(Value::as_i64).unwrap_or(0))
                    });
                    self.state.history = list;
                }
                self.check_render_finished();
            }
            1051 => {
                if error_code != 0 {
                    self.render = None;
                    self.toast(
                        ToastKind::Error,
                        format!("Timelapse render refused: {}", codes::error_code_name(error_code)),
                    );
                }
            }
            1044 => {
                if let Some(list) = result.get("file_list").and_then(Value::as_array) {
                    self.state.files = list.clone();
                    self.state.files_media = self.ui.files_media.to_string();
                } else if error_code != 0 {
                    self.state.files.clear();
                    self.toast(
                        ToastKind::Warning,
                        format!("File list: {}", codes::error_code_name(error_code)),
                    );
                }
            }
            1045 => {
                let target = id.and_then(|id| self.thumb_requests.remove(&id));
                let tex = (error_code == 0)
                    .then(|| result.get("thumbnail").and_then(Value::as_str))
                    .flatten()
                    .and_then(decode_thumbnail)
                    .map(|img| self.ctx.load_texture("thumbnail", img, egui::TextureOptions::LINEAR));
                match target {
                    Some(ThumbFor::PrintDialog) => {
                        if let Some(job) = &mut self.print_job {
                            job.thumb = tex;
                        }
                    }
                    Some(ThumbFor::CurrentJob) | None => {
                        if tex.is_some() {
                            self.thumb = tex;
                        }
                    }
                }
            }
            1046 => {
                let file = result.get("filename").and_then(Value::as_str).unwrap_or_default();
                if let Some(job) = &mut self.print_job {
                    // An unrelated/stale 1046 reply must never populate a new print dialog.
                    if matches!(job.stage, Stage::LoadingDetails { .. })
                        && job.details_request_id.is_some()
                        && job.details_request_id == id
                        && (file.is_empty() || file == job.filename)
                    {
                        if result.get("color_map").and_then(Value::as_array).is_none() {
                            job.stage = Stage::DetailsFailed("Printer returned no verifiable G-code color map. Retry file details; printing without a verified map is blocked.".into());
                        } else if self.state.canvas.get("canvas_list").and_then(Value::as_array).is_none() {
                            job.stage = Stage::DetailsFailed("CANVAS status is unavailable. Retry after the printer reports its slots.".into());
                        } else {
                            job.set_detail(result.clone(), &self.state.canvas);
                        }
                    }
                }
                // Only the file being printed may set the job's layer count.
                let current = self.state.s("print_status.filename").unwrap_or_default();
                if file.is_empty() || file == current {
                    let layers = ["TotalLayers", "layer", "total_layer"]
                        .iter()
                        .find_map(|k| result.get(*k).and_then(Value::as_i64))
                        .filter(|&n| n > 0);
                    if layers.is_some() {
                        self.state.file_total_layers = layers;
                    }
                }
            }
            1020 => {
                let starting = self
                    .print_job
                    .as_ref()
                    .is_some_and(|j| matches!(j.stage, Stage::Starting { request_id, .. } if Some(request_id) == id));
                if !starting { return; }
                if result.get("error_code").and_then(Value::as_i64) == Some(0) {
                    if starting {
                        let name = self.print_job.take().map(|j| j.filename).unwrap_or_default();
                        self.toast(ToastKind::Success, format!("Print started: {name}"));
                    }
                } else {
                    let reason = codes::error_code_name(error_code);
                    match &mut self.print_job {
                        Some(job) if starting => {
                            job.stage = Stage::Ready;
                            job.error = Some(format!("The printer refused to start: {reason}"));
                        }
                        _ => self.toast(ToastKind::Error, format!("Start print failed: {reason}")),
                    }
                }
            }
            _ => {
                if error_code != 0 {
                    if let Some(label) = codes::command_label(method) {
                        let kind = if error_code == 1009 {
                            ToastKind::Warning
                        } else {
                            ToastKind::Error
                        };
                        self.toast(
                            kind,
                            format!("{label} failed: {}", codes::error_code_name(error_code)),
                        );
                    }
                }
            }
        }
    }

    /// Follow-ups when the job changes, plus a few notable transitions.
    fn after_status_change(&mut self) {
        if let Some(name) = self.state.s("print_status.filename").filter(|n| !n.is_empty()) {
            if self.thumb_for.as_deref() != Some(name) {
                let name = name.to_string();
                self.thumb = None;
                self.state.file_total_layers = None;
                if let Some(id) = self.send(1045, json!({"storage_media": "local", "file_name": name})) {
                    self.thumb_requests.insert(id, ThumbFor::CurrentJob);
                }
                self.send(1046, json!({"storage_media": "local", "filename": name}));
                self.thumb_for = Some(name);
            }
        }

        let sub = self.state.sub_status();
        if sub != self.last_sub_status {
            // Only announce transitions, not the state found on connect.
            if self.last_sub_status != 0 {
                match sub {
                    2077 => self.toast(ToastKind::Success, "Print complete"),
                    2502 => self.toast(ToastKind::Warning, "Print paused"),
                    2504 => self.toast(ToastKind::Info, "Print stopped"),
                    2505 => self.toast(ToastKind::Error, "Filament interruption"),
                    _ => {}
                }
            }
            self.last_sub_status = sub;
        }
    }

    fn pump_camera(&mut self, ctx: &egui::Context) {
        let Some(cam) = &self.camera else { return };
        if let Some(frame) = cam.take_frame() {
            match &mut self.cam_tex {
                Some(tex) => tex.set(frame.image, egui::TextureOptions::LINEAR),
                None => {
                    self.cam_tex =
                        Some(ctx.load_texture("camera", frame.image, egui::TextureOptions::LINEAR))
                }
            }
            self.last_jpeg = Some(frame.jpeg);
        }
    }

    // ---- printing -------------------------------------------------------

    /// Open the native file picker on a worker thread (it blocks).
    pub fn pick_gcode_file(&mut self) {
        if self.file_pick.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Send G-code to printer")
                .add_filter("G-code", &["gcode"])
                .pick_file();
            let _ = tx.send(picked);
            ctx.request_repaint();
        });
        self.file_pick = Some(rx);
    }

    /// Upload a local file, then continue into the print dialog.
    pub fn send_file(&mut self, path: PathBuf) {
        let is_gcode = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("gcode"));
        if !is_gcode {
            self.toast(ToastKind::Warning, "Only .gcode files can be sent to the printer");
            return;
        }
        if !self.is_ready() {
            self.toast(ToastKind::Warning, "Connect to the printer first");
            return;
        }
        if self.print_job.is_some() {
            self.toast(ToastKind::Warning, "Finish or close the current print dialog first");
            return;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return;
        };

        let progress = Arc::new(Progress::default());
        let (tx, done) = mpsc::channel();
        let host = format!("{}:{}", self.settings.ip.trim(), upload::UPLOAD_PORT);
        let token = match self.settings.access_code.trim() {
            "" => crate::config::DEFAULT_ACCESS_CODE.to_string(),
            code => code.to_string(),
        };
        {
            let progress = progress.clone();
            let (path, name, ctx) = (path.clone(), name.clone(), self.ctx.clone());
            std::thread::spawn(move || {
                let result = upload::upload_file(&host, &token, &path, &name, &progress);
                let _ = tx.send(result);
                ctx.request_repaint();
            });
        }
        let mut job = PrintJob::new(name, "local", Stage::Uploading { progress, done });
        job.local_path = Some(path);
        job.smooth_plate = self.settings.smooth_plate;
        self.print_job = Some(job);
    }

    /// Open the print dialog for a file already on the printer or USB drive.
    pub fn print_existing(&mut self, filename: String, media: &'static str) {
        if self.print_job.is_some() {
            return;
        }
        let mut job = PrintJob::new(filename, media, Stage::LoadingDetails { since: Instant::now() });
        job.smooth_plate = self.settings.smooth_plate;
        self.print_job = Some(job);
        self.request_job_details();
    }

    fn request_job_details(&mut self) {
        let Some(job) = &self.print_job else { return };
        let (name, media) = (job.filename.clone(), job.media);
        let detail_id = self.send(1046, json!({"storage_media": media, "filename": name}));
        if let Some(job) = &mut self.print_job {
            job.details_request_id = detail_id;
            if detail_id.is_none() {
                job.stage = Stage::DetailsFailed("Cannot request file details from the printer. Check connection and retry.".into());
            }
        }
        if let Some(id) = self.send(1045, json!({"storage_media": media, "file_name": name})) {
            self.thumb_requests.insert(id, ThumbFor::PrintDialog);
        }
    }

    pub fn retry_job_details(&mut self) {
        if let Some(job) = &mut self.print_job {
            if !matches!(job.stage, Stage::DetailsFailed(_)) { return; }
            job.stage = Stage::LoadingDetails { since: Instant::now() };
            job.details_request_id = None;
        } else { return; }
        self.send(2005, json!({}));
        self.request_job_details();
    }

    fn refill_error(&mut self, message: &str) {
        if let Some(job) = &mut self.print_job {
            job.stage = Stage::Ready;
            job.error = Some(message.into());
        }
    }

    fn send_start_command(&mut self) {
        if !self.printer_idle() {
            self.refill_error("Printer is not idle; start cancelled.");
            return;
        }
        let Some(job) = &self.print_job else { return };
        if !job.mapping_complete() { self.refill_error("Filament mapping is no longer valid."); return; }
        // Revalidate the previously chosen physical trays against the latest
        // CANVAS status, especially after a 2005 refresh. Never silently use a
        // slot whose material/color changed since the dialog opened.
        if job.canvas_connected && !job.use_external_spool {
            let live_trays = crate::print_job::trays_from_canvas(&self.state.canvas);
            let unchanged = job.mapping.iter().all(|selection| {
                selection.and_then(|i| job.trays.get(i)).is_some_and(|saved| {
                    live_trays.iter().any(|live| {
                        live.canvas_id == saved.canvas_id && live.tray_id == saved.tray_id
                            && live.kind.eq_ignore_ascii_case(&saved.kind)
                            && live.color.eq_ignore_ascii_case(&saved.color)
                    })
                })
            });
            if !unchanged {
                self.refill_error("CANVAS slots changed since file details were loaded. Reopen the file and verify material mapping.");
                return;
            }
        }
        let params = job.start_params();
        if let Some(request_id) = self.send(1020, params) {
            if let Some(job) = &mut self.print_job {
                job.error = None;
                job.stage = Stage::Starting { since: Instant::now(), request_id };
            }
        } else {
            self.refill_error("Failed to queue start-print command.");
        }
    }

    /// Send the start command. Refuses unless the printer is idle.
    pub fn start_print(&mut self) {
        if !self.printer_idle() {
            return;
        }
        let Some(job) = &self.print_job else { return };
        if !matches!(job.stage, Stage::Ready) || !job.mapping_complete() {
            return;
        }
        let (smooth, refill, printer_refill, external, canvas_connected) =
            (job.smooth_plate, job.auto_refill, job.printer_auto_refill, job.use_external_spool, job.canvas_connected);
        self.settings.smooth_plate = smooth;
        // Do not start until the printer acknowledges and reports the desired
        // persistent CANVAS setting. Silent failures are unsafe for auto-refill.
        if !external && refill != printer_refill {
            if let Some(request_id) = self.send(2004, json!({"enable": refill})) {
                if let Some(job) = &mut self.print_job {
                    job.error = None;
                    job.stage = Stage::SettingRefill { since: Instant::now(), request_id, expected: refill };
                }
            } else {
                self.refill_error("Failed to queue CANVAS auto-refill setting.");
            }
        } else if !external && canvas_connected {
            // Re-read CANVAS even when auto-refill did not change, because its
            // slots or the persistent setting may have changed since dialog load.
            if let Some(request_id) = self.send(2005, json!({})) {
                if let Some(job) = &mut self.print_job {
                    job.error = None;
                    job.stage = Stage::VerifyingRefill { since: Instant::now(), request_id, expected: refill };
                }
            } else {
                self.refill_error("Cannot verify current CANVAS status before printing.");
            }
        } else {
            self.send_start_command();
        }
    }

    pub fn cancel_print_job(&mut self) {
        if let Some(job) = self.print_job.take() {
            if let Stage::Uploading { progress, .. } = &job.stage {
                progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// Advance uploads, picker results and timeouts.
    fn poll_print_job(&mut self) {
        if let Some(rx) = &self.file_pick {
            if let Ok(picked) = rx.try_recv() {
                self.file_pick = None;
                if let Some(path) = picked {
                    self.send_file(path);
                }
            }
        }

        let Some(job) = &mut self.print_job else { return };
        match &job.stage {
            Stage::Uploading { done, .. } => {
                if let Ok(result) = done.try_recv() {
                    match result {
                        Ok(()) => {
                            job.stage = Stage::LoadingDetails { since: Instant::now() };
                            let name = job.filename.clone();
                            self.toast(ToastKind::Success, format!("Uploaded {name}"));
                            self.request_job_details();
                            if self.state.files_media == "local" {
                                self.send(
                                    1044,
                                    json!({"storage_media": "local", "dir": "", "offset": 0, "limit": 200}),
                                );
                            }
                        }
                        Err(e) => job.stage = Stage::Failed(e),
                    }
                }
            }
            Stage::LoadingDetails { since } if since.elapsed() > DETAIL_TIMEOUT => {
                job.stage = Stage::DetailsFailed("File detail request timed out. Retry; start is blocked until material mapping is verified.".into());
            }
            Stage::SettingRefill { since, .. } | Stage::VerifyingRefill { since, .. }
                if since.elapsed() > REFILL_TIMEOUT => {
                job.stage = Stage::Ready;
                job.error = Some("Timed out confirming CANVAS auto-refill. Start cancelled; verify the printer setting.".into());
            }
            Stage::Starting { since } if since.elapsed() > START_TIMEOUT => {
                job.stage = Stage::Ready;
                job.error = Some("No answer from the printer. Check its screen before retrying.".into());
            }
            _ => {}
        }
    }

    // ---- timelapses ----------------------------------------------------

    fn access_token(&self) -> String {
        match self.settings.access_code.trim() {
            "" => crate::config::DEFAULT_ACCESS_CODE.to_string(),
            code => code.to_string(),
        }
    }

    /// Ask where to save a rendered timelapse, then download it.
    pub fn download_timelapse(&mut self, task_id: String, remote: String) {
        if self.downloads.contains_key(&task_id) || self.save_pick.is_some() {
            return;
        }
        if self.state.is_printing() {
            self.toast(ToastKind::Warning, "Timelapses can be downloaded once the printer is idle");
            return;
        }
        let mut name = remote.rsplit('/').next().unwrap_or("timelapse").to_string();
        if !name.to_lowercase().ends_with(".mp4") {
            name.push_str(".mp4");
        }
        let dir = dirs::video_dir().or_else(dirs::home_dir).unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Save timelapse")
                .set_directory(dir)
                .set_file_name(name)
                .add_filter("MP4 video", &["mp4"])
                .save_file();
            let _ = tx.send(picked);
            ctx.request_repaint();
        });
        self.save_pick = Some((task_id, remote, rx));
    }

    fn start_download(&mut self, task_id: String, remote: String, dest: PathBuf) {
        let progress = Arc::new(Progress::default());
        let (tx, done) = mpsc::channel();
        let host = format!("{}:{}", self.settings.ip.trim(), upload::UPLOAD_PORT);
        let token = self.access_token();
        {
            let (progress, dest, ctx) = (progress.clone(), dest.clone(), self.ctx.clone());
            std::thread::spawn(move || {
                let result = download::download_file(
                    &host,
                    &token,
                    &remote,
                    &dest,
                    &progress,
                    download::READ_TIMEOUT,
                );
                let _ = tx.send(result);
                ctx.request_repaint();
            });
        }
        self.saved.remove(&task_id);
        self.downloads.insert(task_id, Transfer { progress, done, dest });
    }

    pub fn cancel_download(&mut self, task_id: &str) {
        if let Some(t) = self.downloads.remove(task_id) {
            t.progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Have the printer build the MP4 from a print's raw frames. The printer
    /// deletes the frames afterwards, so each print can be rendered once.
    pub fn render_timelapse(&mut self, task_id: String, url: String) {
        if !self.printer_idle() || self.render.is_some() {
            return;
        }
        if self.send(1051, json!({"url": url})).is_some() {
            let now = Instant::now();
            self.render = Some(RenderJob {
                task_id,
                since: now,
                last_poll: now,
                start_sub_status: self.state.sub_status(),
            });
        }
    }

    /// The render is done once history lists the task's video as ready.
    fn check_render_finished(&mut self) {
        let Some(job) = &self.render else { return };
        let ready = self.state.history.iter().any(|e| {
            e.get("task_id").and_then(Value::as_str) == Some(job.task_id.as_str())
                && e.get("time_lapse_video_status").and_then(Value::as_i64) == Some(2)
        });
        if ready {
            self.render = None;
            self.toast(ToastKind::Success, "Timelapse rendered and ready to download");
        }
    }

    fn poll_timelapses(&mut self) {
        if let Some((_, _, rx)) = &self.save_pick {
            if let Ok(picked) = rx.try_recv() {
                let (task_id, remote, _) = self.save_pick.take().expect("checked above");
                if let Some(dest) = picked {
                    self.start_download(task_id, remote, dest);
                }
            }
        }

        let mut done = Vec::new();
        for (id, t) in &self.downloads {
            match t.done.try_recv() {
                Ok(result) => done.push((id.clone(), Some(result))),
                Err(mpsc::TryRecvError::Disconnected) => done.push((id.clone(), None)),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        for (id, result) in done {
            let Some(t) = self.downloads.remove(&id) else { continue };
            match result {
                Some(Ok(())) => {
                    self.toast(ToastKind::Success, format!("Saved {}", t.dest.display()));
                    self.saved.insert(id, t.dest);
                }
                Some(Err(e)) if e == "Download cancelled" => {}
                Some(Err(e)) => self.toast(ToastKind::Error, e),
                None => self.toast(ToastKind::Error, "Download stopped unexpectedly"),
            }
        }

        let Some(job) = &self.render else { return };
        let sub = self.state.sub_status();
        if sub == SUB_TIMELAPSE_NO_FRAMES && job.start_sub_status != SUB_TIMELAPSE_NO_FRAMES {
            self.render = None;
            self.toast(ToastKind::Warning, "The printer has no frames left to render for that print");
        } else if job.since.elapsed() > RENDER_TIMEOUT {
            self.render = None;
            self.toast(ToastKind::Error, "Timelapse render did not finish in 5 minutes");
        } else if job.last_poll.elapsed() > RENDER_POLL && self.state.machine_status() != Some(12) {
            // Not busy composing: see whether history now lists the video.
            if let Some(job) = &mut self.render {
                job.last_poll = Instant::now();
            }
            self.send(1036, json!({}));
        }
    }

    pub fn save_snapshot(&mut self) {
        let Some(jpeg) = self.last_jpeg.clone() else {
            self.toast(ToastKind::Warning, "No camera frame to save yet");
            return;
        };
        let dir = dirs::picture_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_default()
            .join("CC2 Monitor");
        let name = chrono::Local::now().format("cc2-%Y%m%d-%H%M%S.jpg").to_string();
        let path = dir.join(name);
        match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, jpeg)) {
            Ok(()) => self.toast(ToastKind::Success, format!("Saved {}", path.display())),
            Err(e) => self.toast(ToastKind::Error, format!("Snapshot failed: {e}")),
        }
    }
}

/// Show a saved file in the system file manager.
pub fn reveal_in_folder(path: &Path) {
    #[cfg(windows)]
    {
        // Let Command escape the file path rather than embedding it in a raw
        // Windows command-line argument.
        let _ = std::process::Command::new("explorer")
            .arg("/select,")
            .arg(path)
            .spawn();
    }
    #[cfg(not(windows))]
    if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

fn decode_thumbnail(b64: &str) -> Option<egui::ColorImage> {
    let data = b64.split_once("base64,").map_or(b64, |(_, d)| d);
    if data.len() > 12_000_000 { return None; }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()?;
    let probe = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format().ok()?;
    let (width, height) = probe.into_dimensions().ok()?;
    if (width as u64) * (height as u64) > 8_000_000 { return None; }
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [img.width() as usize, img.height() as usize],
        img.as_raw(),
    ))
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_events();
        self.poll_print_job();
        self.poll_timelapses();
        self.pump_camera(ctx);

        if self.last_sample.elapsed() >= Duration::from_secs(1) {
            self.last_sample = Instant::now();
            self.state.sample_temps();
        }
        self.toasts.retain(|t| t.at.elapsed() < TOAST_TTL);
        for p in self.ui.fan_pending.iter_mut() {
            if p.is_some_and(|p| p.at.elapsed() > Duration::from_secs(6)) {
                *p = None;
            }
        }

        ui::draw(self, ctx);

        // Keep the clock, chart and countdowns moving between status updates.
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.settings.protected_access_code =
            crate::secret::protect(&self.settings.access_code).unwrap_or_default();
        eframe::set_value(storage, STORAGE_KEY, &self.settings);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Free the printer's client slot (it only allows two) right away.
        self.client = None;
        self.camera = None;
    }
}
