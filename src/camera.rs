//! Live camera feed.
//!
//! The CC2 serves an MJPEG stream (`multipart/x-mixed-replace`) on :8080 for
//! any path. Frames are cut out of the byte stream by their JPEG start/end
//! markers, decoded on this worker thread, and handed to the UI one at a time.
//!
//! CPU is spent mostly on redrawing the window once per delivered frame, not on
//! decoding, so frames are dropped *before* decoding when they exceed the frame
//! rate cap or when the UI has not drawn the previous frame yet (for example
//! while the window is minimized).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use image::RgbImage;

use crate::ai::{Detection, FrameAnalyzer};

pub const CAMERA_PORT: u16 = 8080;
const MAX_BUFFER: usize = 16 * 1024 * 1024;

/// Frame rate choices for the quick settings menu; 0 means as fast as the camera sends.
pub const FPS_CHOICES: [u32; 6] = [0, 15, 10, 5, 2, 1];
/// Resolution divisors for the quick settings menu.
pub const SCALE_CHOICES: [(u8, &str); 3] = [(1, "Full"), (2, "Half"), (4, "Quarter")];

#[derive(Clone, Debug, PartialEq)]
pub enum CamStatus {
    Connecting,
    Streaming,
    Error(String),
}

pub struct Frame {
    pub image: egui::ColorImage,
    /// The original JPEG, kept so snapshots are saved at full quality.
    pub jpeg: Vec<u8>,
}

struct Shared {
    latest: Mutex<Option<Frame>>,
    status: Mutex<CamStatus>,
    fps: Mutex<f32>,
    source_size: Mutex<Option<[usize; 2]>>,
    detection: Mutex<Option<Detection>>,
    max_fps: AtomicU32,
    scale: AtomicU8,
    stop: AtomicBool,
}

pub struct CameraStream {
    shared: Arc<Shared>,
}

impl CameraStream {
    pub fn start(
        ip: &str,
        ctx: egui::Context,
        analyzer: Box<dyn FrameAnalyzer>,
        max_fps: u32,
        scale: u8,
    ) -> Self {
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            status: Mutex::new(CamStatus::Connecting),
            fps: Mutex::new(0.0),
            source_size: Mutex::new(None),
            detection: Mutex::new(None),
            max_fps: AtomicU32::new(max_fps),
            scale: AtomicU8::new(scale.max(1)),
            stop: AtomicBool::new(false),
        });
        let worker = Worker {
            ip: ip.trim().to_string(),
            shared: shared.clone(),
            ctx,
            analyzer,
        };
        thread::Builder::new()
            .name("cc2-camera".into())
            .spawn(move || worker.run())
            .expect("spawn camera thread");
        Self { shared }
    }

    /// Change the frame rate cap (0 = unlimited) and resolution divisor live.
    pub fn set_limits(&self, max_fps: u32, scale: u8) {
        self.shared.max_fps.store(max_fps, Ordering::Relaxed);
        self.shared.scale.store(scale.max(1), Ordering::Relaxed);
    }

    /// The newest decoded frame, if one arrived since the last call.
    pub fn take_frame(&self) -> Option<Frame> {
        self.shared.latest.lock().unwrap().take()
    }

    pub fn status(&self) -> CamStatus {
        self.shared.status.lock().unwrap().clone()
    }

    /// Frames delivered to the UI per second.
    pub fn fps(&self) -> f32 {
        *self.shared.fps.lock().unwrap()
    }

    /// Resolution the camera sends, before any scaling.
    pub fn source_size(&self) -> Option<[usize; 2]> {
        *self.shared.source_size.lock().unwrap()
    }

    #[allow(dead_code)] // read by the future AI panel
    pub fn detection(&self) -> Option<Detection> {
        self.shared.detection.lock().unwrap().clone()
    }
}

impl Drop for CameraStream {
    fn drop(&mut self) {
        // The worker notices within one socket read timeout and exits.
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

struct Worker {
    ip: String,
    shared: Arc<Shared>,
    ctx: egui::Context,
    analyzer: Box<dyn FrameAnalyzer>,
}

impl Worker {
    fn stopped(&self) -> bool {
        self.shared.stop.load(Ordering::Relaxed)
    }

    fn set_status(&self, s: CamStatus) {
        let mut cur = self.shared.status.lock().unwrap();
        if *cur != s {
            *cur = s;
            self.ctx.request_repaint();
        }
    }

    fn run(mut self) {
        while !self.stopped() {
            self.set_status(CamStatus::Connecting);
            if let Err(e) = self.stream_once() {
                if self.stopped() {
                    break;
                }
                self.set_status(CamStatus::Error(e.to_string()));
            }
            *self.shared.fps.lock().unwrap() = 0.0;
            for _ in 0..30 {
                if self.stopped() {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    fn stream_once(&mut self) -> std::io::Result<()> {
        let addr: SocketAddr = (self.ip.as_str(), CAMERA_PORT)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other("invalid camera address"))?;
        let mut tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
        tcp.set_nodelay(true)?;
        write!(
            tcp,
            "GET /?action=stream HTTP/1.1\r\nHost: {}:{CAMERA_PORT}\r\nUser-Agent: cc2-monitor\r\nConnection: close\r\n\r\n",
            self.ip
        )?;

        let mut buf: Vec<u8> = Vec::with_capacity(512 * 1024);
        let mut chunk = vec![0u8; 64 * 1024];
        let mut last_delivered: Option<Instant> = None;
        let mut window_start = Instant::now();
        let mut window_frames = 0u32;

        loop {
            if self.stopped() {
                return Ok(());
            }
            let n = tcp.read(&mut chunk)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "camera closed the stream",
                ));
            }
            buf.extend_from_slice(&chunk[..n]);
            self.set_status(CamStatus::Streaming);

            let elapsed = window_start.elapsed();
            if elapsed >= Duration::from_secs(1) {
                *self.shared.fps.lock().unwrap() = window_frames as f32 / elapsed.as_secs_f32();
                window_frames = 0;
                window_start = Instant::now();
            }

            let Some(jpeg) = extract_latest_jpeg(&mut buf) else {
                if buf.len() > MAX_BUFFER {
                    buf.clear();
                }
                continue;
            };

            // Frame rate cap. The 0.8 tolerance absorbs network jitter so a
            // 10 fps cap on a 20 fps camera really delivers ~10, not ~7.
            let max_fps = self.shared.max_fps.load(Ordering::Relaxed);
            if max_fps > 0 {
                let interval = Duration::from_secs_f32(0.8 / max_fps as f32);
                if last_delivered.is_some_and(|t| t.elapsed() < interval) {
                    continue;
                }
            }
            // The UI has not drawn the previous frame (minimized or busy):
            // decoding another one would be wasted work.
            if !self.analyzer.enabled() && self.shared.latest.lock().unwrap().is_some() {
                continue;
            }

            let scale = self.shared.scale.load(Ordering::Relaxed);
            let Some((rgb, source)) = decode(&jpeg, scale) else {
                continue; // a torn frame; the next one will do
            };
            *self.shared.source_size.lock().unwrap() = Some(source);

            if self.analyzer.enabled() {
                *self.shared.detection.lock().unwrap() = self.analyzer.analyze(&rgb);
            }

            let size = [rgb.width() as usize, rgb.height() as usize];
            let image = egui::ColorImage::from_rgb(size, rgb.as_raw());
            *self.shared.latest.lock().unwrap() = Some(Frame { image, jpeg });
            last_delivered = Some(Instant::now());
            window_frames += 1;
            self.ctx.request_repaint();
        }
    }
}

/// Decode a JPEG, shrinking it by `scale` (1, 2, 4 or 8) during the IDCT,
/// which is much cheaper than decoding at full size and resizing.
/// Returns the image and the camera's original resolution.
fn decode(jpeg: &[u8], scale: u8) -> Option<(RgbImage, [usize; 2])> {
    let mut d = jpeg_decoder::Decoder::new(jpeg);
    d.read_info().ok()?;
    let info = d.info()?;
    // Bound decoded pixels, not just the compressed MJPEG buffer.
    if (info.width as u64) * (info.height as u64) > 8_000_000 {
        return None;
    }
    let source = [info.width as usize, info.height as usize];
    let (w, h) = if scale > 1 {
        d.scale(info.width / scale as u16, info.height / scale as u16)
            .ok()?
    } else {
        (info.width, info.height)
    };
    let pixels = d.decode().ok()?;
    let rgb = match d.info()?.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => RgbImage::from_raw(w as u32, h as u32, pixels)?,
        jpeg_decoder::PixelFormat::L8 => {
            let expanded = pixels.iter().flat_map(|&l| [l, l, l]).collect();
            RgbImage::from_raw(w as u32, h as u32, expanded)?
        }
        _ => return None,
    };
    Some((rgb, source))
}

/// Remove every complete JPEG from the front of `buf` and return the last one.
fn extract_latest_jpeg(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let mut latest = None;
    loop {
        let Some(start) = find(buf, &[0xFF, 0xD8]) else {
            // Keep a trailing 0xFF in case it is the first half of a marker.
            let keep = usize::from(buf.last() == Some(&0xFF));
            let drop_to = buf.len() - keep;
            buf.drain(..drop_to);
            break;
        };
        let Some(rel_end) = find(&buf[start + 2..], &[0xFF, 0xD9]) else {
            buf.drain(..start);
            break;
        };
        let end = start + 2 + rel_end + 2;
        latest = Some(buf[start..end].to_vec());
        buf.drain(..end);
    }
    latest
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_frames_across_boundaries() {
        let mut buf = b"--frame\r\nContent-Type: image/jpeg\r\n\r\n".to_vec();
        buf.extend_from_slice(&[0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9]);
        buf.extend_from_slice(b"\r\n--frame\r\n\r\n");
        buf.extend_from_slice(&[0xFF, 0xD8, 9, 9]);

        assert_eq!(extract_latest_jpeg(&mut buf), Some(vec![0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9]));
        assert_eq!(buf, vec![0xFF, 0xD8, 9, 9]);

        buf.extend_from_slice(&[8, 0xFF, 0xD9]);
        assert_eq!(extract_latest_jpeg(&mut buf), Some(vec![0xFF, 0xD8, 9, 9, 8, 0xFF, 0xD9]));
        assert!(buf.is_empty());
    }

    #[test]
    fn decodes_at_reduced_scale() {
        let mut jpeg = Vec::new();
        RgbImage::from_pixel(64, 32, image::Rgb([200, 30, 30]))
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        let (full, src) = decode(&jpeg, 1).unwrap();
        assert_eq!((full.width(), full.height(), src), (64, 32, [64, 32]));
        let (half, _) = decode(&jpeg, 2).unwrap();
        assert_eq!((half.width(), half.height()), (32, 16));
        let (quarter, _) = decode(&jpeg, 4).unwrap();
        assert_eq!((quarter.width(), quarter.height()), (16, 8));
    }
}
