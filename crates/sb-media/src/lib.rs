//! sb-media: media probing, thumbnail extraction, and the filesystem cache.
//!
//! PLAN.md §6 (media levels), §7 (sidecar cache), §8 (filesystem-first),
//! §15 (media backend spike: start with external ffmpeg/ffprobe).
//!
//! A small worker pool extracts one representative frame per clip via the
//! `ffmpeg` CLI into a content-addressed sidecar cache, then decodes it to
//! RGBA for the renderer's atlas. The render thread never blocks on this.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::UNIX_EPOCH;

/// Generation parameters, chosen by the app from tuning at startup.
/// Cache artifact names encode size + quality + grid, so changing the
/// recipe regenerates rather than serving stale files.
#[derive(Debug, Clone, Copy)]
pub struct Recipe {
    /// Thumbs fit within this box, aspect preserved (= atlas slot size).
    pub thumb_w: u32,
    pub thumb_h: u32,
    /// ffmpeg -q:v — 2 ≈ visually lossless, 31 = worst.
    pub quality: u8,
    /// Anim sheets are `anim_grid × anim_grid` crop-filled frames sampled
    /// evenly across the clip (PLAN.md §6 level 2), packed into one slot.
    /// Frame resolution = thumb / anim_grid: 3 = more motion, 2 = crisper.
    pub anim_grid: u32,
}

impl Recipe {
    fn thumb_file(&self) -> String {
        format!("thumb_fit_{}x{}_q{}.jpg", self.thumb_w, self.thumb_h, self.quality)
    }
    fn anim_file(&self) -> String {
        let g = self.anim_grid;
        format!("anim_{g}x{g}_{}x{}_q{}.jpg", self.thumb_w, self.thumb_h, self.quality)
    }
    fn anim_frame(&self) -> (u32, u32) {
        let g = self.anim_grid.max(1);
        ((self.thumb_w / g).max(2), (self.thumb_h / g).max(2))
    }
}

const WORKERS: usize = 3;
/// Extract the frame this far into the clip (PLAN.md §6 initial policy).
/// Public so live playback can start from the same frame the thumb shows.
pub const SEEK_FRACTION: f64 = 0.10;

enum Request {
    Thumb(PathBuf),
    Anim(PathBuf),
}

/// Two-priority work queue: static thumbs always dispatch before anim
/// sheets, so scrolling into fresh territory never waits on animations.
#[derive(Default)]
struct Queues {
    thumbs: VecDeque<PathBuf>,
    anims: VecDeque<PathBuf>,
}

type SharedQueue = Arc<(Mutex<Queues>, Condvar)>;

pub enum ThumbResult {
    /// `rgba` is `w × h × 4` bytes, fitting the recipe's thumb box with
    /// the clip's original aspect ratio.
    Ready { path: PathBuf, w: u32, h: u32, rgba: Vec<u8> },
    Failed { path: PathBuf },
    /// A sprite sheet of ANIM_FRAMES crop-filled frames; `w × h` are the
    /// sheet dimensions (frame size = w/ANIM_COLS × h/ANIM_ROWS).
    AnimReady { path: PathBuf, w: u32, h: u32, rgba: Vec<u8> },
    AnimFailed { path: PathBuf },
}

/// Async thumbnail service. `request` from the UI thread, results arrive
/// via `try_recv` on later frames.
pub struct MediaService {
    queue: SharedQueue,
    rx: Receiver<ThumbResult>,
}

impl MediaService {
    pub fn new(recipe: Recipe) -> Self {
        let queue: SharedQueue = Arc::new((Mutex::new(Queues::default()), Condvar::new()));
        let (tx_done, rx_done) = mpsc::channel::<ThumbResult>();

        let have_ffmpeg = have_binary("ffmpeg") && have_binary("ffprobe");
        if !have_ffmpeg {
            log::warn!(
                "ffmpeg/ffprobe not found on PATH — thumbnail generation disabled, \
                 tiles stay placeholders (cached thumbnails still load)"
            );
        }
        let root = cache_root();

        for _ in 0..WORKERS {
            let q = queue.clone();
            let tx = tx_done.clone();
            let root = root.clone();
            thread::spawn(move || worker(q, tx, root, have_ffmpeg, recipe));
        }
        Self { queue, rx: rx_done }
    }

    pub fn request(&self, path: PathBuf) {
        let (lock, cv) = &*self.queue;
        lock.lock().unwrap().thumbs.push_back(path);
        cv.notify_one();
    }

    pub fn request_anim(&self, path: PathBuf) {
        let (lock, cv) = &*self.queue;
        lock.lock().unwrap().anims.push_back(path);
        cv.notify_one();
    }

    pub fn try_recv(&self) -> Option<ThumbResult> {
        self.rx.try_recv().ok()
    }
}

/// Live playback for the selected clip (PLAN.md §6 level 3): an ffmpeg
/// child decodes to raw RGBA on stdout at native pace (`-re`), looping
/// forever; a reader thread keeps only the latest frame. One instance at a
/// time, killed on drop. Software decode of a single ≤640px stream is
/// cheap; hardware decode can slot in later without changing the interface.
pub struct LivePlayer {
    child: std::process::Child,
    frame: Arc<Mutex<Option<Vec<u8>>>>,
    pub w: u32,
    pub h: u32,
}

impl LivePlayer {
    /// `seek` starts playback that many seconds in — pass the thumbnail's
    /// frame time so live video continues from what the tile showed.
    pub fn spawn(path: &Path, w: u32, h: u32, seek: f64) -> Option<Self> {
        let (w, h) = (w.max(2), h.max(2));
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-stream_loop", "-1", "-re"]);
        if seek > 0.05 {
            cmd.args(["-ss", &format!("{seek:.3}")]);
        }
        let mut child = cmd
            .arg("-i")
            .arg(path)
            .args([
                "-an",
                "-sn",
                "-vf",
                &format!("scale={w}:{h}"),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdout = child.stdout.take()?;
        let frame = Arc::new(Mutex::new(None));
        let latest = frame.clone();
        let frame_bytes = (w * h * 4) as usize;
        thread::spawn(move || {
            use std::io::Read;
            let mut buf = vec![0u8; frame_bytes];
            loop {
                if stdout.read_exact(&mut buf).is_err() {
                    return; // EOF or killed
                }
                *latest.lock().unwrap() = Some(buf.clone());
            }
        });
        Some(Self { child, frame, w, h })
    }

    /// The newest decoded frame, if one arrived since the last call.
    pub fn take_frame(&self) -> Option<Vec<u8>> {
        self.frame.lock().unwrap().take()
    }
}

impl Drop for LivePlayer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn worker(
    queue: SharedQueue,
    tx: Sender<ThumbResult>,
    root: PathBuf,
    have_ffmpeg: bool,
    recipe: Recipe,
) {
    loop {
        // Thumbs always beat anims; the lock drops before any work starts.
        let req = {
            let (lock, cv) = &*queue;
            let mut q = lock.lock().unwrap();
            loop {
                if let Some(p) = q.thumbs.pop_front() {
                    break Request::Thumb(p);
                }
                if let Some(p) = q.anims.pop_front() {
                    break Request::Anim(p);
                }
                q = cv.wait(q).unwrap();
            }
        };
        let result = match req {
            Request::Thumb(path) => match make_thumb(&path, &root, have_ffmpeg, &recipe) {
                Some((w, h, rgba)) => ThumbResult::Ready { path, w, h, rgba },
                None => ThumbResult::Failed { path },
            },
            Request::Anim(path) => match make_anim(&path, &root, have_ffmpeg, &recipe) {
                Some((w, h, rgba)) => ThumbResult::AnimReady { path, w, h, rgba },
                None => ThumbResult::AnimFailed { path },
            },
        };
        if tx.send(result).is_err() {
            return;
        }
    }
}

/// Serve from the sidecar cache, generating on miss. See PLAN.md §7/§8.
fn make_thumb(
    src: &Path,
    root: &Path,
    have_ffmpeg: bool,
    recipe: &Recipe,
) -> Option<(u32, u32, Vec<u8>)> {
    let meta = std::fs::metadata(src).ok()?;
    if !meta.is_file() {
        return None;
    }
    let fp = fingerprint(src, meta.len(), mtime_secs(&meta));
    let dir = root.join(&fp[..2]).join(&fp);
    let jpg = dir.join(recipe.thumb_file());

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
        extract_frame(src, &jpg, seek, recipe)?;
        log::debug!("thumb generated: {}", src.display());
    }
    decode_jpeg(&jpg, recipe.thumb_w, recipe.thumb_h)
}

/// Generate/serve the animated sprite sheet: ANIM_FRAMES frames sampled
/// evenly across the clip, crop-filled to 16:9, tiled into one JPEG.
fn make_anim(
    src: &Path,
    root: &Path,
    have_ffmpeg: bool,
    recipe: &Recipe,
) -> Option<(u32, u32, Vec<u8>)> {
    let meta = std::fs::metadata(src).ok()?;
    if !meta.is_file() {
        return None;
    }
    let fp = fingerprint(src, meta.len(), mtime_secs(&meta));
    let dir = root.join(&fp[..2]).join(&fp);
    let jpg = dir.join(recipe.anim_file());

    if !jpg.exists() {
        if !have_ffmpeg {
            return None;
        }
        std::fs::create_dir_all(&dir).ok()?;
        // Sampling frames across the clip needs its duration.
        let duration = probe(src).and_then(|m| m.duration).filter(|d| *d > 0.05)?;
        let tmp = jpg.with_extension("jpg.tmp");
        // Slightly overshoot the rate so the tile filter always fills all
        // cells before EOF (a padded black cell looks broken).
        let g = recipe.anim_grid.max(1);
        let fps = ((g * g) as f64 + 0.5) / duration;
        let (fw, fh) = recipe.anim_frame();
        let q = recipe.quality.clamp(2, 31).to_string();
        let vf = format!(
            "fps={fps:.6},scale={fw}:{fh}:force_original_aspect_ratio=increase,\
             crop={fw}:{fh},tile={g}x{g}"
        );
        let out = Command::new("ffmpeg")
            .args(["-y", "-v", "error"])
            .arg("-i")
            .arg(src)
            .args(["-frames:v", "1", "-vf", &vf, "-q:v", &q, "-strict", "unofficial", "-f", "mjpeg"])
            .arg(&tmp)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() || !tmp.exists() {
            let _ = std::fs::remove_file(&tmp);
            let stderr = String::from_utf8_lossy(&out.stderr);
            log::debug!(
                "ffmpeg could not build anim sheet for {}: {}",
                src.display(),
                stderr.lines().last().unwrap_or("(no output)")
            );
            return None;
        }
        std::fs::rename(&tmp, &jpg).ok()?;
        log::debug!("anim sheet generated: {}", src.display());
    }
    decode_jpeg(&jpg, recipe.thumb_w, recipe.thumb_h)
}

/// Cached probe results — a snapshot for humans and future features.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Meta {
    pub src: PathBuf,
    pub duration: Option<f64>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub codec: Option<String>,
    pub fps: Option<f64>,
}

/// Read the cached meta.json for a clip without probing (cheap: one small
/// file read; no process spawn). Present for anything with a thumbnail.
pub fn cached_meta(path: &Path) -> Option<Meta> {
    let meta = std::fs::metadata(path).ok()?;
    let fp = fingerprint(path, meta.len(), mtime_secs(&meta));
    let file = cache_root().join(&fp[..2]).join(&fp).join("meta.json");
    serde_json::from_slice(&std::fs::read(file).ok()?).ok()
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

fn extract_frame(src: &Path, dst: &Path, seek: f64, recipe: &Recipe) -> Option<()> {
    // Write to a temp name and rename, so a half-written jpg never
    // looks like a cache hit to another worker or a later run.
    let tmp = dst.with_extension("jpg.tmp");
    let (tw, th) = (recipe.thumb_w, recipe.thumb_h);
    let q = recipe.quality.clamp(2, 31).to_string();
    let vf = format!("scale={tw}:{th}:force_original_aspect_ratio=decrease");
    // stderr is captured, not inherited: decode noise from damaged files
    // must never spam the console (it becomes one debug line below).
    // `-strict unofficial` lets mjpeg accept full-range YUV sources
    // (common in phone and AI-generated footage; hard error in ffmpeg 8+).
    let out = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-ss", &format!("{seek:.3}")])
        .arg("-i")
        .arg(src)
        .args([
            // the mjpeg default quality is very blocky; 2 ≈ visually lossless
            "-frames:v", "1", "-vf", &vf, "-q:v", &q, "-strict", "unofficial", "-f", "mjpeg",
        ])
        .arg(&tmp)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    // A seek past EOF exits 0 without producing a file: retry from the start.
    if !out.status.success() || !tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
        if seek > 0.0 {
            return extract_frame(src, dst, 0.0, recipe);
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

/// Decode a cached artifact to RGBA **via ffmpeg**, so thumbnails go
/// through the exact same YUV→RGB conversion as live playback. Decoding
/// JPEG with a JFIF-assuming image library uses subtly different color
/// math (BT.601 full-range vs the source matrix) — enough for a visible
/// gamma/color pop the moment live video replaces the thumb.
fn decode_jpeg(path: &Path, max_w: u32, max_h: u32) -> Option<(u32, u32, Vec<u8>)> {
    // Header-only dimension read; no full decode.
    let (mut w, mut h) = image::image_dimensions(path).ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error"]).arg("-i").arg(path);
    if w > max_w || h > max_h {
        // Oversized (foreign/stale artifact): scale down, keep aspect.
        let s = (max_w as f32 / w as f32).min(max_h as f32 / h as f32);
        w = ((w as f32 * s) as u32).max(1);
        h = ((h as f32 * s) as u32).max(1);
        cmd.args(["-vf", &format!("scale={w}:{h}")]);
    }
    let out = cmd
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.len() != (w * h * 4) as usize {
        return None;
    }
    Some((w, h, out.stdout))
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
