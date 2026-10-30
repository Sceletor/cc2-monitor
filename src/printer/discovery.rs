//! LAN discovery: the CC2 answers `{"id":0,"method":7000}` on UDP :52700.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub const DISCOVERY_PORT: u16 = 52700;

#[derive(Clone, Debug)]
pub struct Discovered {
    pub ip: String,
    pub name: String,
    pub model: String,
    pub serial: String,
    /// 1 when an access code is set on the printer.
    pub token_status: i64,
}

/// Broadcast on the local network and collect answers until `timeout`.
pub fn scan(timeout: Duration) -> std::io::Result<Vec<Discovered>> {
    query((Ipv4Addr::BROADCAST, DISCOVERY_PORT).into(), timeout, true)
}

/// Ask one known address directly.
pub fn probe(ip: &str, timeout: Duration) -> Option<Discovered> {
    let addr: Ipv4Addr = ip.trim().parse().ok()?;
    query((addr, DISCOVERY_PORT).into(), timeout, false)
        .ok()?
        .into_iter()
        .next()
}

fn query(target: SocketAddr, timeout: Duration, broadcast: bool) -> std::io::Result<Vec<Discovered>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_broadcast(broadcast)?;
    sock.send_to(json!({"id": 0, "method": 7000}).to_string().as_bytes(), target)?;

    let deadline = Instant::now() + timeout;
    let mut found: Vec<Discovered> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        sock.set_read_timeout(Some(left.max(Duration::from_millis(10))))?;
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(r) => r,
            Err(_) => break,
        };
        // A directed probe must never accept a forged response from a different host.
        if !broadcast && from.ip() != target.ip() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&buf[..n]) else {
            continue;
        };
        let Some(r) = v.get("result") else { continue };
        let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        // Ignore arbitrary UDP traffic lacking a printer serial number.
        if s("sn").is_empty() {
            continue;
        }
        let ip = from.ip().to_string();
        if found.iter().any(|d| d.ip == ip) {
            continue;
        }
        found.push(Discovered {
            ip,
            name: s("host_name"),
            model: s("machine_model"),
            serial: s("sn"),
            token_status: r.get("token_status").and_then(Value::as_i64).unwrap_or(0),
        });
        if !broadcast {
            break;
        }
    }
    Ok(found)
}
