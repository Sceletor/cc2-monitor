//! MQTT session with a CC2 printer.
//!
//! Handshake (per the CC2 protocol):
//! 1. Connect to the printer's broker on :1883 as `elegoo` / access code.
//! 2. Publish `{client_id, request_id}` to `elegoo/<sn>/api_register`, wait for
//!    `elegoo/<sn>/<request_id>/register_response` with `error: "ok"`.
//! 3. Subscribe to `elegoo/<sn>/api_status` (deltas) and
//!    `elegoo/<sn>/<client_id>/api_response` (replies to our commands).
//! 4. Send commands to `elegoo/<sn>/<client_id>/api_request`, PING periodically.
//!
//! The printer accepts at most two registered clients; a third gets `code: 3`.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use rumqttc::{Client, Event as MqttEvent, MqttOptions, Packet, QoS};
use serde_json::{json, Value};

use super::discovery;

pub const MQTT_PORT: u16 = 1883;
const REGISTER_RETRY: Duration = Duration::from_secs(5);
const REGISTER_RETRY_REJECTED: Duration = Duration::from_secs(30);
const PING_INTERVAL: Duration = Duration::from_secs(10);
const MAX_MISSED_PINGS: u32 = 3;
/// Thumbnails and history arrive as single large JSON messages.
const MAX_PACKET: usize = 8 * 1024 * 1024;
const LOG_BODY_LIMIT: usize = 1500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    Disconnected,
    AwaitingSn,
    Registering,
    Rejected,
    Connected,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Connecting => "Connecting…",
            Phase::Disconnected => "Disconnected",
            Phase::AwaitingSn => "Waiting for printer",
            Phase::Registering => "Registering…",
            Phase::Rejected => "Refused (too many clients)",
            Phase::Connected => "Connected",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Phase::Connecting => "Opening a connection to the printer's MQTT broker.",
            Phase::Disconnected => "No connection to the printer. Retrying every few seconds.",
            Phase::AwaitingSn => {
                "The broker answered but the printer has not spoken yet. Enter its serial \
                 number, or power-cycle it if this persists."
            }
            Phase::Registering => "Registering with the printer.",
            Phase::Rejected => {
                "The printer already has its maximum of two clients. Close Elegoo Slicer's \
                 device tab or another monitor; this app retries every 30 s."
            }
            Phase::Connected => "Registered and receiving status.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Sent,
    Received,
    Info,
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub at: SystemTime,
    pub dir: Dir,
    pub topic: String,
    pub method: Option<i64>,
    pub body: String,
}

pub enum Event {
    Phase(Phase),
    /// A full `api_status` message (`result` holds the delta).
    Status(Value),
    /// A reply on our own `api_response` topic.
    Response { method: i64, msg: Value },
    SnLearned(String),
    Log(LogEntry),
}

#[derive(Clone)]
pub struct ConnectConfig {
    pub ip: String,
    pub access_code: String,
    /// Serial number if known; otherwise it is learned from UDP discovery or
    /// from the first message the printer publishes.
    pub serial: String,
}

struct Shared {
    mqtt: Client,
    client_id: String,
    request_id: String,
    sn: Mutex<String>,
    tx: Sender<Event>,
    ctx: egui::Context,
    next_id: AtomicU64,
    broker_up: AtomicBool,
    ever_failed: AtomicBool,
    registered: AtomicBool,
    rejected: AtomicBool,
    wide_subscribed: AtomicBool,
    missed_pings: AtomicU32,
    register_attempts: AtomicU32,
    last_register: Mutex<Option<Instant>>,
    phase: Mutex<Phase>,
    stop: AtomicBool,
}

pub struct PrinterClient {
    shared: Arc<Shared>,
    events: Receiver<Event>,
    threads: Vec<JoinHandle<()>>,
}

impl PrinterClient {
    pub fn connect(cfg: ConnectConfig, ctx: egui::Context) -> Self {
        let client_id = random_hex(10);
        let request_id = random_hex(26);

        let mut opts = MqttOptions::new(client_id.clone(), cfg.ip.clone(), MQTT_PORT);
        opts.set_credentials("elegoo", cfg.access_code.clone());
        opts.set_keep_alive(Duration::from_secs(60));
        opts.set_clean_session(true);
        opts.set_max_packet_size(MAX_PACKET, 64 * 1024);

        let (mqtt, mut connection) = Client::new(opts, 64);
        let (tx, events) = mpsc::channel();

        let shared = Arc::new(Shared {
            mqtt,
            client_id,
            request_id,
            sn: Mutex::new(cfg.serial.trim().to_string()),
            tx,
            ctx,
            next_id: AtomicU64::new(1),
            broker_up: AtomicBool::new(false),
            ever_failed: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            rejected: AtomicBool::new(false),
            wide_subscribed: AtomicBool::new(false),
            missed_pings: AtomicU32::new(0),
            register_attempts: AtomicU32::new(0),
            last_register: Mutex::new(None),
            phase: Mutex::new(Phase::Connecting),
            stop: AtomicBool::new(false),
        });
        shared.info(format!("Connecting to mqtt://{}:{MQTT_PORT}", cfg.ip));

        let net = {
            let shared = shared.clone();
            let ip = cfg.ip.clone();
            thread::Builder::new()
                .name("cc2-mqtt".into())
                .spawn(move || {
                    // An idle printer publishes nothing until a client registers,
                    // and registering needs the serial, so ask for it over UDP.
                    if shared.sn().is_empty() {
                        if let Some(found) = discovery::probe(&ip, Duration::from_millis(1500)) {
                            if !found.serial.is_empty() {
                                shared.info(format!("UDP discovery: serial {}", found.serial));
                                *shared.sn.lock().unwrap() = found.serial.clone();
                                shared.emit(Event::SnLearned(found.serial));
                            }
                        }
                    }
                    for notification in connection.iter() {
                        if shared.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        match notification {
                            Ok(MqttEvent::Incoming(Packet::ConnAck(_))) => {
                                shared.on_broker_connected()
                            }
                            Ok(MqttEvent::Incoming(Packet::Publish(p))) => {
                                shared.on_publish(&p.topic, &p.payload)
                            }
                            Ok(_) => {}
                            Err(e) => {
                                shared.on_connection_error(e.to_string());
                                // rumqttc reconnects on the next poll; pace it.
                                for _ in 0..30 {
                                    if shared.stop.load(Ordering::Relaxed) {
                                        return;
                                    }
                                    thread::sleep(Duration::from_millis(100));
                                }
                            }
                        }
                    }
                })
                .expect("spawn mqtt thread")
        };

        let ticker = {
            let shared = shared.clone();
            thread::Builder::new()
                .name("cc2-heartbeat".into())
                .spawn(move || shared.tick_loop())
                .expect("spawn heartbeat thread")
        };

        Self {
            shared,
            events,
            threads: vec![net, ticker],
        }
    }

    pub fn poll(&self) -> impl Iterator<Item = Event> + '_ {
        self.events.try_iter()
    }

    pub fn is_ready(&self) -> bool {
        self.shared.registered.load(Ordering::Relaxed)
    }

    /// Send a command and return its request id, which the printer echoes in
    /// the reply. `None` if not registered yet (the command is dropped).
    pub fn send(&self, method: i64, params: Value) -> Option<u64> {
        self.is_ready().then(|| self.shared.send(method, params)).flatten()
    }
}

impl Drop for PrinterClient {
    /// Disconnect cleanly so the printer frees our client slot right away
    /// instead of waiting for the keep-alive to expire.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.shared.mqtt.try_disconnect();
        let deadline = Instant::now() + Duration::from_millis(800);
        while Instant::now() < deadline && self.threads.iter().any(|t| !t.is_finished()) {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Shared {
    fn emit(&self, ev: Event) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }

    fn log(&self, dir: Dir, topic: &str, method: Option<i64>, body: &str) {
        let body = if body.len() > LOG_BODY_LIMIT {
            let mut cut = LOG_BODY_LIMIT;
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}… ({} bytes)", &body[..cut], body.len())
        } else {
            body.to_string()
        };
        self.emit(Event::Log(LogEntry {
            at: SystemTime::now(),
            dir,
            topic: topic.to_string(),
            method,
            body,
        }));
    }

    fn info(&self, msg: impl AsRef<str>) {
        self.log(Dir::Info, "", None, msg.as_ref());
    }

    fn sn(&self) -> String {
        self.sn.lock().unwrap().clone()
    }

    fn update_phase(&self) {
        let phase = if !self.broker_up.load(Ordering::Relaxed) {
            if self.ever_failed.load(Ordering::Relaxed) {
                Phase::Disconnected
            } else {
                Phase::Connecting
            }
        } else if self.registered.load(Ordering::Relaxed) {
            Phase::Connected
        } else if self.rejected.load(Ordering::Relaxed) {
            Phase::Rejected
        } else if self.sn().is_empty() {
            Phase::AwaitingSn
        } else {
            Phase::Registering
        };
        let mut cur = self.phase.lock().unwrap();
        if *cur != phase {
            *cur = phase;
            drop(cur);
            self.emit(Event::Phase(phase));
        }
    }

    fn on_broker_connected(&self) {
        self.broker_up.store(true, Ordering::Relaxed);
        self.registered.store(false, Ordering::Relaxed);
        self.rejected.store(false, Ordering::Relaxed);
        self.wide_subscribed.store(false, Ordering::Relaxed);
        self.register_attempts.store(0, Ordering::Relaxed);
        self.info("Broker connected");
        if self.sn().is_empty() {
            self.info("Serial unknown, listening for the printer to announce itself");
            self.subscribe_wide();
        } else {
            self.register();
        }
        self.update_phase();
    }

    fn on_connection_error(&self, err: String) {
        let was_up = self.broker_up.swap(false, Ordering::Relaxed);
        self.registered.store(false, Ordering::Relaxed);
        self.ever_failed.store(true, Ordering::Relaxed);
        if !self.stop.load(Ordering::Relaxed) {
            let msg = if was_up {
                format!("Connection lost: {err}")
            } else {
                format!("Connect failed: {err}")
            };
            self.info(msg);
        }
        self.update_phase();
    }

    /// Catch-all subscription used only to learn the printer's serial.
    fn subscribe_wide(&self) {
        if !self.wide_subscribed.swap(true, Ordering::Relaxed) {
            let _ = self.mqtt.try_subscribe("elegoo/#", QoS::AtLeastOnce);
        }
    }

    fn register(&self) {
        let sn = self.sn();
        if sn.is_empty() {
            return;
        }
        let _ = self.mqtt.try_subscribe(
            format!("elegoo/{sn}/{}/register_response", self.request_id),
            QoS::AtLeastOnce,
        );
        let topic = format!("elegoo/{sn}/api_register");
        let body = json!({"client_id": self.client_id, "request_id": self.request_id}).to_string();
        self.log(Dir::Sent, &topic, None, &body);
        let _ = self
            .mqtt
            .try_publish(topic, QoS::AtMostOnce, false, body.into_bytes());
        *self.last_register.lock().unwrap() = Some(Instant::now());
    }

    fn on_registered(&self) {
        let sn = self.sn();
        self.registered.store(true, Ordering::Relaxed);
        self.rejected.store(false, Ordering::Relaxed);
        self.missed_pings.store(0, Ordering::Relaxed);
        self.register_attempts.store(0, Ordering::Relaxed);
        let _ = self
            .mqtt
            .try_subscribe(format!("elegoo/{sn}/api_status"), QoS::AtLeastOnce);
        let _ = self.mqtt.try_subscribe(
            format!("elegoo/{sn}/{}/api_response", self.client_id),
            QoS::AtLeastOnce,
        );
        if self.wide_subscribed.swap(false, Ordering::Relaxed) {
            let _ = self.mqtt.try_unsubscribe("elegoo/#");
        }
        self.info(format!("Registered with printer {sn}"));
        self.send(1001, json!({})); // attributes
        self.send(1002, json!({})); // full status
        self.send(2005, json!({})); // Canvas (AMS)
        self.send(1036, json!({})); // print history
        self.update_phase();
    }

    fn on_publish(&self, topic: &str, payload: &[u8]) {
        let Ok(text) = std::str::from_utf8(payload) else {
            return;
        };
        let Ok(msg) = serde_json::from_str::<Value>(text) else {
            return;
        };

        // Our own publishes echo back through the wide subscription; skip them.
        if topic.ends_with("/api_request") || topic.ends_with("/api_register") {
            return;
        }

        let method = msg.get("method").and_then(Value::as_i64);
        self.log(Dir::Received, topic, method, text);

        // Learn (or correct) the serial from anything the printer publishes.
        if !self.registered.load(Ordering::Relaxed) {
            if let Some(sn) = topic.strip_prefix("elegoo/").and_then(|t| t.split('/').next()) {
                if !sn.is_empty() && sn != self.sn() {
                    *self.sn.lock().unwrap() = sn.to_string();
                    self.info(format!("Discovered printer serial {sn}"));
                    self.emit(Event::SnLearned(sn.to_string()));
                    self.register();
                    self.update_phase();
                }
            }
        }

        if topic.ends_with("/register_response") {
            // Compare the entire topic; a substring match is not sufficient.
            if topic != format!("elegoo/{}/{}/register_response", self.sn(), self.request_id) {
                return;
            }
            if msg.get("error").and_then(Value::as_str) == Some("ok") {
                if !self.registered.load(Ordering::Relaxed) {
                    self.on_registered();
                }
            } else if msg.get("code").and_then(Value::as_i64) == Some(3) {
                if !self.rejected.swap(true, Ordering::Relaxed) {
                    self.info("Registration refused: printer already has 2 clients");
                }
                self.update_phase();
            }
        } else if topic == format!("elegoo/{}/{}/api_response", self.sn(), self.client_id) {
            self.missed_pings.store(0, Ordering::Relaxed);
            if let Some(method) = method {
                self.emit(Event::Response { method, msg });
            }
        } else if topic == format!("elegoo/{}/api_status", self.sn()) {
            self.emit(Event::Status(msg));
        }
    }

    fn send(&self, method: i64, params: Value) -> Option<u64> {
        let sn = self.sn();
        if sn.is_empty() {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let topic = format!("elegoo/{sn}/{}/api_request", self.client_id);
        let body = json!({"id": id, "method": method, "params": params}).to_string();
        // Only report a request ID when MQTT accepted it into the local queue.
        // QoS 0 is still not delivery confirmation; critical actions must await a reply.
        match self.mqtt.try_publish(topic.clone(), QoS::AtMostOnce, false, body.clone().into_bytes()) {
            Ok(()) => {
                self.log(Dir::Sent, &topic, Some(method), &body);
                Some(id)
            }
            Err(e) => {
                self.info(format!("Failed to queue {method}: {e}"));
                None
            }
        }
    }

    fn tick_loop(&self) {
        let mut last_ping = Instant::now();
        while !self.stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(250));
            if !self.broker_up.load(Ordering::Relaxed) {
                continue;
            }

            if !self.registered.load(Ordering::Relaxed) {
                if self.sn().is_empty() {
                    continue;
                }
                let interval = if self.rejected.load(Ordering::Relaxed) {
                    REGISTER_RETRY_REJECTED
                } else {
                    REGISTER_RETRY
                };
                let due = self
                    .last_register
                    .lock()
                    .unwrap()
                    .is_none_or(|t| t.elapsed() >= interval);
                if due {
                    let attempts = self.register_attempts.fetch_add(1, Ordering::Relaxed) + 1;
                    // A stale or mistyped serial never gets an answer; listen
                    // broadly so the real one can be picked up.
                    if attempts >= 3 {
                        self.subscribe_wide();
                    }
                    self.register();
                }
                continue;
            }

            if last_ping.elapsed() >= PING_INTERVAL {
                last_ping = Instant::now();
                let missed = self.missed_pings.fetch_add(1, Ordering::Relaxed);
                if missed >= MAX_MISSED_PINGS {
                    self.info("Heartbeat lost, re-registering");
                    self.registered.store(false, Ordering::Relaxed);
                    self.missed_pings.store(0, Ordering::Relaxed);
                    self.update_phase();
                    continue;
                }
                let sn = self.sn();
                let _ = self.mqtt.try_publish(
                    format!("elegoo/{sn}/{}/api_request", self.client_id),
                    QoS::AtMostOnce,
                    false,
                    json!({"type": "PING"}).to_string().into_bytes(),
                );
            }
        }
    }
}

fn random_hex(len: usize) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut out = String::with_capacity(len + 16);
    while out.len() < len {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.write_usize(out.len());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out.truncate(len);
    out
}
