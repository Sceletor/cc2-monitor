//! File download over HTTP: `GET http://<printer>:80/download?X-Token=<code>&file_name=<name>`.
//!
//! Used for timelapse MP4s, whose `file_name` is the `video/<name>.mp4` path from
//! print history (method 1036). A file that no longer exists does not get a 404:
//! the printer accepts the request and then never answers, so a read timeout
//! before the response arrives means "gone".

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::upload::Progress;

/// How long to wait for the printer to start answering (and between chunks).
pub const READ_TIMEOUT: Duration = Duration::from_secs(8);

/// The printer accepted the request but never answered. Measured on fw
/// 02.01.01.07: this happens for files deleted on the touchscreen, and also for
/// every file while a print is running.
const NO_ANSWER: &str = "The printer didn't send the file. It may have been deleted on the \
     touchscreen, or the printer may be busy printing. Try again when it's idle.";

/// Download `remote` to `dest`. Writes to `<dest>.part` and renames on
/// success, so a cancelled or failed download never leaves a broken file.
pub fn download_file(
    host: &str,
    token: &str,
    remote: &str,
    dest: &Path,
    progress: &Progress,
    read_timeout: Duration,
) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(read_timeout)
        .redirects(0)
        .build();
    let resp = agent
        .get(&format!("http://{host}/download"))
        .set("User-Agent", "cc2-monitor")
        .query("X-Token", token)
        .query("file_name", remote)
        .call();
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => return Err(format!("Printer answered HTTP {code}")),
        Err(e) if is_timeout(&e) => {
            return Err(NO_ANSWER.into())
        }
        // Not `{e}`: ureq's message includes the URL, and with it the access code.
        Err(ureq::Error::Transport(t)) => {
            return Err(format!("Download failed: {}", t.kind()))
        }
    };

    let total = resp
        .header("Content-Length")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    progress.total.store(total, Ordering::Relaxed);

    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let result = (|| {
        let mut out = File::create(&part).map_err(|e| format!("Cannot write file: {e}"))?;
        let mut body = resp.into_reader();
        let mut buf = vec![0u8; 64 * 1024];
        let mut sent = 0u64;
        loop {
            if progress.cancel.load(Ordering::Relaxed) {
                return Err("Download cancelled".to_string());
            }
            let n = body
                .read(&mut buf)
                .map_err(|e| format!("Download interrupted: {e}"))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])
                .map_err(|e| format!("Cannot write file: {e}"))?;
            sent += n as u64;
            progress.sent.store(sent, Ordering::Relaxed);
        }
        if total > 0 && sent != total {
            return Err(format!("Download incomplete ({sent} of {total} bytes)"));
        }
        out.flush().map_err(|e| format!("Cannot write file: {e}"))
    })();

    match result {
        Ok(()) => std::fs::rename(&part, dest).map_err(|e| format!("Cannot save file: {e}")),
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

/// True when the request died waiting for the printer to answer.
fn is_timeout(e: &ureq::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                return true;
            }
        }
        source = err.source();
    }
    let text = e.to_string().to_lowercase();
    text.contains("timed out") || text.contains("os error 10060")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cc2-dl-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn downloads_with_query_and_progress() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        let served = body.clone();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            loop {
                let mut l = String::new();
                reader.read_line(&mut l).unwrap();
                if l == "\r\n" {
                    break;
                }
            }
            let mut s = stream;
            write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", served.len()).unwrap();
            s.write_all(&served).unwrap();
            request_line
        });

        let dir = temp_dir("ok");
        let dest = dir.join("clip.mp4");
        let progress = Progress::default();
        download_file(&host, "123456", "video/my print.mp4", &dest, &progress, READ_TIMEOUT).unwrap();

        let request_line = server.join().unwrap();
        assert!(request_line.starts_with("GET /download?"), "{request_line}");
        assert!(request_line.contains("X-Token=123456"));
        assert!(request_line.contains("file_name=video%2Fmy"), "{request_line}");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(progress.fraction(), 1.0);
        assert!(!dir.join("clip.mp4.part").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn silent_printer_means_file_is_gone() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        // Accept and say nothing, as the printer does for a missing file.
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(800));
            drop(stream);
        });
        let dir = temp_dir("gone");
        let dest = dir.join("clip.mp4");
        let err = download_file(&host, "123456", "video/x.mp4", &dest, &Progress::default(), Duration::from_millis(300))
            .unwrap_err();
        assert_eq!(err, NO_ANSWER);
        assert!(!dest.exists());
        server.join().unwrap();
        std::fs::remove_dir_all(dir).ok();
    }
}
