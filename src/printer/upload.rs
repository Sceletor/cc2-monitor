//! G-code upload over HTTP, as Elegoo Slicer does it (elegoo-link SDK).
//!
//! `PUT http://<printer>:80/upload` in chunks of at most 1 MB. Every chunk
//! carries `Content-Range: bytes <start>-<end>/<total>`, the file name, the MD5
//! of the whole file, and the access code as `X-Token`. The printer replies
//! `{"error_code": 0, ...}` per chunk; 1000 means the token was rejected.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

pub const UPLOAD_PORT: u16 = 80;
const CHUNK: usize = 1024 * 1024;

#[derive(Default)]
pub struct Progress {
    pub sent: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        let total = self.total.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        self.sent.load(Ordering::Relaxed) as f32 / total as f32
    }
}

/// Upload `path` to the printer's local storage as `remote_name`.
/// `host` is `ip:port` (port [`UPLOAD_PORT`] on a real printer).
/// Blocking; run it on a worker thread.
pub fn upload_file(
    host: &str,
    token: &str,
    path: &Path,
    remote_name: &str,
    progress: &Progress,
) -> Result<(), String> {
    let total = std::fs::metadata(path)
        .map_err(|e| format!("Cannot read file: {e}"))?
        .len();
    if total == 0 {
        return Err("The file is empty".into());
    }
    progress.total.store(total, Ordering::Relaxed);
    let md5 = file_md5(path).map_err(|e| format!("Cannot read file: {e}"))?;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(180))
        .timeout_write(Duration::from_secs(180))
        .redirects(0)
        .build();
    let url = format!("http://{host}/upload");

    let mut file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
    let mut buf = vec![0u8; CHUNK];
    let mut offset: u64 = 0;
    while offset < total {
        if progress.cancel.load(Ordering::Relaxed) {
            return Err("Upload cancelled".into());
        }
        let len = ((total - offset) as usize).min(CHUNK);
        file.read_exact(&mut buf[..len])
            .map_err(|e| format!("Cannot read file: {e}"))?;
        let end = offset + len as u64 - 1;

        let resp = agent
            .put(&url)
            .set("User-Agent", "cc2-monitor")
            .set("Accept", "application/json")
            .set("Content-Type", "application/octet-stream")
            .set("Content-Range", &format!("bytes {offset}-{end}/{total}"))
            .set("X-File-Name", remote_name)
            .set("X-File-MD5", &md5)
            .set("X-Token", token)
            .send_bytes(&buf[..len]);
        let body = match resp {
            Ok(r) => r.into_string().map_err(|e| format!("Bad reply from printer: {e}"))?,
            Err(ureq::Error::Status(401 | 403, _)) => {
                return Err("The printer rejected the access code".into())
            }
            Err(ureq::Error::Status(code, _)) => return Err(format!("Printer answered HTTP {code}")),
            Err(e) => return Err(format!("Upload failed: {e}")),
        };
        check_reply(&body)?;

        offset += len as u64;
        progress.sent.store(offset, Ordering::Relaxed);
    }
    Ok(())
}

fn check_reply(body: &str) -> Result<(), String> {
    let v: Value =
        serde_json::from_str(body).map_err(|_| format!("Unexpected reply from printer: {body}"))?;
    match v.get("error_code").and_then(Value::as_i64) {
        Some(0) => Ok(()),
        Some(1000) => Err("The printer rejected the access code".into()),
        Some(code) => Err(format!(
            "Printer refused the upload: {} ({code})",
            super::codes::error_code_name(code)
        )),
        None => Err(format!("Unexpected reply from printer: {body}")),
    }
}

fn file_md5(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut ctx = md5::Context::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.consume(&buf[..n]);
    }
    Ok(format!("{:x}", ctx.compute()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// A fake printer: records each PUT's headers and body size, answers ok.
    #[test]
    fn uploads_in_chunks_with_elegoo_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut headers = Vec::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    headers.push(line.trim_end().to_string());
                }
                let len: usize = headers
                    .iter()
                    .find_map(|h| h.to_lowercase().strip_prefix("content-length: ").map(|v| v.parse().unwrap()))
                    .unwrap();
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).unwrap();
                let reply = r#"{"error_code":0}"#;
                write!(
                    &stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
                seen.push((headers, len));
            }
            seen
        });

        // 2.5 MB → chunks of 1 MB, 1 MB, 0.5 MB.
        let dir = std::env::temp_dir().join(format!("cc2-upload-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("part.gcode");
        let data: Vec<u8> = (0..(CHUNK * 5 / 2)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let progress = Progress::default();
        upload_file(&host, "123456", &path, "part.gcode", &progress).unwrap();
        assert_eq!(progress.fraction(), 1.0);

        let seen = server.join().unwrap();
        let md5 = format!("{:x}", md5::compute(&data));
        let total = data.len();
        let expect = [
            (format!("bytes 0-{}/{total}", CHUNK - 1), CHUNK),
            (format!("bytes {CHUNK}-{}/{total}", 2 * CHUNK - 1), CHUNK),
            (format!("bytes {}-{}/{total}", 2 * CHUNK, total - 1), total - 2 * CHUNK),
        ];
        for ((headers, len), (range, want_len)) in seen.iter().zip(expect) {
            assert!(headers[0].starts_with("PUT /upload HTTP/1.1"), "{headers:?}");
            let has = |h: &str| headers.iter().any(|x| x.eq_ignore_ascii_case(h));
            assert!(has(&format!("content-range: {range}")), "{headers:?}");
            assert!(has("x-file-name: part.gcode"));
            assert!(has(&format!("x-file-md5: {md5}")));
            assert!(has("x-token: 123456"));
            assert_eq!(*len, want_len);
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn token_rejection_is_reported() {
        assert!(check_reply(r#"{"error_code":1000}"#).unwrap_err().contains("access code"));
        assert!(check_reply(r#"{"error_code":0,"offset":9}"#).is_ok());
    }
}
