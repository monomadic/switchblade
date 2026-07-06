//! sb-media: media probing, thumbnail extraction, and the filesystem cache.
//!
//! PLAN.md §6 (media levels), §7 (sidecar cache), §8 (filesystem-first),
//! §15 (media backend spike: start with external ffmpeg/ffprobe).
//!
//! A small worker pool extracts one representative frame per clip via the
//! `ffmpeg` CLI into a content-addressed sidecar cache, then decodes it to
//! RGBA for the renderer's atlas. The render thread never blocks on this.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::UNIX_EPOCH;

/// Thumbnails fit within this box with their aspect ratio preserved; the
/// grid crops to tile shape at sampling time, so the selected tile can show
/// the whole frame. 640×360 keeps tiles crisp on 2x displays.
pub const THUMB_W: u32 = 640;
pub const THUMB_H: u32 = 360;

/// Cache artifact name. Bumped when the recipe changes (thumb.jpg =
/// crop-filled 480; thumb_fit.jpg = default-quality 480): stale artifacts
/// are simply ignored and regenerated.
const THUMB_FILE: &str = "thumb_fit_640.jpg";

const WORKERS: usize = 3;
/// Extract the frame this far into the clip (PLAN.md §6 initial policy).
const SEEK_FRACTION: f64 = 0.10;

pub enum ThumbResult {
    /// `rgba` is `w × h × 4` bytes, with `w ≤ THUMB_W`, `h ≤ THUMB_H` and
    /// the clip's original aspect ratio.
    Ready { path: PathBuf, w: u32, h: u32, rgba: Vec<u8> },
    Failed { path: PathBuf },
}

/// Async thumbnail service. `request` from the UI thread, results arrive
/// via `try_recv` on later frames.
pub struct MediaService {
    tx: Sender<PathBuf>,
    rx: Receiver<ThumbResult>,
}

impl Default for MediaService {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaService {
    pub fn new() -> Self {
        let (tx_req, rx_req) = mpsc::channel::<PathBuf>();
        let (tx_done, rx_done) = mpsc::channel::<ThumbResult>();
        let rx_req = Arc::new(Mutex::new(rx_req));

        let have_ffmpeg = have_binary("ffmpeg") && have_binary("ffprobe");
        if !have_ffmpeg {
            log::warn!(
                "ffmpeg/ffprobe not found on PATH — thumbnail generation disabled, \
                 tiles stay placeholders (cached thumbnails still load)"
            );
        }
        let root = cache_root();

        for _ in 0..WORKERS {
            let rx = rx_req.clone();
            let tx = tx_done.clone();
            let root = root.clone();
            thread::spawn(move || worker(rx, tx, root, have_ffmpeg));
        }
        Self { tx: tx_req, rx: rx_done }
    }

    pub fn request(&self, path: PathBuf) {
        let _ = self.tx.send(path);
    }

    pub fn try_recv(&self) -> Option<ThumbResult> {
        self.rx.try_recv().ok()
    }
}

fn worker(
    rx: Arc<Mutex<Receiver<PathBuf>>>,
    tx: Sender<ThumbResult>,
    root: PathBuf,
    have_ffmpeg: bool,
) {
    loop {
        // The lock is only held while waiting for the next request; the
        // temporary guard drops before any work starts.
        let path = match { rx.lock().unwrap().recv() } {
            Ok(p) => p,
            Err(_) => return,
        };
        let result = match make_thumb(&path, &root, have_ffmpeg) {
            Some((w, h, rgba)) => ThumbResult::Ready { path, w, h, rgba },
            None => ThumbResult::Failed { path },
        };
        if tx.send(result).is_err() {
            return;
        }
    }
}

/// Serve from the sidecar cache, generating on miss. See PLAN.md §7/§8.
fn make_thumb(src: &Path, root: &Path, have_ffmpeg: bool) -> Option<(u32, u32, Vec<u8>)> {
    let meta = std::fs::metadata(src).ok()?;
    if !meta.is_file() {
        return None;
    }
    let fp = fingerprint(src, meta.len(), mtime_secs(&meta));
    let dir = root.join(&fp[..2]).join(&fp);
    let jpg = dir.join(THUMB_FILE);

    if !jpg.exists() {
        if !have_ffmpeg {
            return None;
        }
        std::fs::create_dir_all(&dir).ok()?;
        let probed = probe(src);
        if let Some(m) = &probed {
            if let Ok(json) = serde_json::to_vec_pretty(m) {
                let _ = std::fs::write(dir.join("meta.json"), json);
            }
        }
        let seek = probed
            .and_then(|m| m.duration)
            .map(|d| (d * SEEK_FRACTION).max(0.0))
            .unwrap_or(0.0);
        extract_frame(src, &jpg, seek)?;
        log::debug!("thumb generated: {}", src.display());
    }
    decode_jpeg(&jpg)
}

/// Cached probe results — a snapshot for humans and future features.
#[derive(serde::Serialize)]
pub struct Meta {
    pub src: PathBuf,
    pub duration: Option<f64>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub codec: Option<String>,
    pub fps: Option<f64>,
}

fn probe(src: &Path) -> Option<Meta> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(src)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let duration = v["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok());
    let video = v["streams"]
        .as_array()
        .and_then(|ss| ss.iter().find(|s| s["codec_type"] == "video"));
    let fps = video
        .and_then(|s| s["avg_frame_rate"].as_str())
        .and_then(|r| {
            let (n, d) = r.split_once('/')?;
            let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
            (d != 0.0).then(|| n / d)
        });
    Some(Meta {
        src: src.to_path_buf(),
        duration,
        width: video.and_then(|s| s["width"].as_u64()),
        height: video.and_then(|s| s["height"].as_u64()),
        codec: video.and_then(|s| s["codec_name"].as_str().map(String::from)),
        fps,
    })
}

fn extract_frame(src: &Path, dst: &Path, seek: f64) -> Option<()> {
    // Write to a temp name and rename, so a half-written jpg never
    // looks like a cache hit to another worker or a later run.
    let tmp = dst.with_extension("jpg.tmp");
    let vf =
        format!("scale={THUMB_W}:{THUMB_H}:force_original_aspect_ratio=decrease");
    // stderr is captured, not inherited: decode noise from damaged files
    // must never spam the console (it becomes one debug line below).
    // `-strict unofficial` lets mjpeg accept full-range YUV sources
    // (common in phone and AI-generated footage; hard error in ffmpeg 8+).
    let out = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-ss", &format!("{seek:.3}")])
        .arg("-i")
        .arg(src)
        .args([
            // -q:v 2 ≈ visually lossless; the mjpeg default is very blocky.
            "-frames:v", "1", "-vf", &vf, "-q:v", "2", "-strict", "unofficial", "-f", "mjpeg",
        ])
        .arg(&tmp)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    // A seek past EOF exits 0 without producing a file: retry from the start.
    if !out.status.success() || !tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
        if seek > 0.0 {
            return extract_frame(src, dst, 0.0);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        log::debug!(
            "ffmpeg could not extract {}: {}",
            src.display(),
            stderr.lines().last().unwrap_or("(no output)")
        );
        return None;
    }
    std::fs::rename(&tmp, dst).ok()
}

fn decode_jpeg(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let img = image::open(path).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    if w <= THUMB_W && h <= THUMB_H {
        return Some((w, h, img.into_raw()));
    }
    // Oversized (foreign/stale artifact): scale down, keep aspect.
    let s = (THUMB_W as f32 / w as f32).min(THUMB_H as f32 / h as f32);
    let (nw, nh) = (((w as f32 * s) as u32).max(1), ((h as f32 * s) as u32).max(1));
    let resized = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
    Some((nw, nh, resized.into_raw()))
}

/// Pragmatic MVP fingerprint: absolute path + size + mtime (PLAN.md §8).
/// FNV-1a so the key is stable across runs and toolchains. Tradeoff: moved
/// files lose their cache; stronger modes (partial hash) come later.
fn fingerprint(path: &Path, size: u64, mtime: u64) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for chunk in [
        path.as_os_str().as_encoded_bytes(),
        &size.to_le_bytes(),
        &mtime.to_le_bytes(),
    ] {
        for &b in chunk {
            h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Platform cache dir (PLAN.md §8): ~/Library/Caches/switchblade on macOS,
/// XDG cache dir elsewhere.
pub fn cache_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(target_os = "macos")]
    let base = home.join("Library/Caches");
    #[cfg(not(target_os = "macos"))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cache"));
    base.join("switchblade").join("v1").join("objects")
}

fn have_binary(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
