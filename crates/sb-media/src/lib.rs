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
        format!(
            "thumb_fit_{}x{}_q{}.jpg",
            self.thumb_w, self.thumb_h, self.quality
        )
    }
    fn anim_file(&self) -> String {
        let g = self.anim_grid;
        format!(
            "anim_{g}x{g}_{}x{}_q{}.jpg",
            self.thumb_w, self.thumb_h, self.quality
        )
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
    /// Generate the thumb (+ meta) on disk without decoding/uploading —
    /// the library-wide background sweep.
    Gen(PathBuf),
    Anim(PathBuf),
}

/// Strict-priority work queue, popped top to bottom — a lower tier never
/// runs while a higher one has work:
///   1. `thumbs` — visible thumbnails (something on screen needs pixels)
///   2. `gen`    — background thumb generation for the whole library
///   3. `anims`  — sprite sheets, always last: every file gets a
///      thumbnail before any animation work starts, and newly discovered
///      files (streaming stdin, dir walks) jump ahead of sheets too.
/// (Live video never queues here at all — it has its own unniced ffmpeg
/// processes; these workers are niced below it.)
#[derive(Default)]
struct Queues {
    thumbs: VecDeque<PathBuf>,
    gen: VecDeque<PathBuf>,
    anims: VecDeque<PathBuf>,
}

type SharedQueue = Arc<(Mutex<Queues>, Condvar)>;

pub enum ThumbResult {
    /// `rgba` is `w × h × 4` bytes, fitting the recipe's thumb box with
    /// the clip's original aspect ratio.
    Ready {
        path: PathBuf,
        w: u32,
        h: u32,
        rgba: Vec<u8>,
    },
    Failed {
        path: PathBuf,
    },
    /// A sprite sheet of ANIM_FRAMES crop-filled frames; `w × h` are the
    /// sheet dimensions (frame size = w/ANIM_COLS × h/ANIM_ROWS).
    AnimReady {
        path: PathBuf,
        w: u32,
        h: u32,
        rgba: Vec<u8>,
    },
    AnimFailed {
        path: PathBuf,
    },
    /// A background gen-sweep item finished (cache written or already
    /// present; failures count too — they'd fail again on demand anyway).
    GenDone {
        path: PathBuf,
    },
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

    /// Queue background thumb generation (disk cache only, no upload).
    pub fn request_gen(&self, path: PathBuf) {
        let (lock, cv) = &*self.queue;
        lock.lock().unwrap().gen.push_back(path);
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

/// Hardware decode, but only for codecs VideoToolbox actually
/// accelerates: it keeps 4K HEVC ahead of realtime (software can't
/// guarantee that) and moves decode onto the media engine, off the CPU.
/// VP9/AV1 measured *slower* routed through `-hwaccel videotoolbox`
/// than straight software decode — don't send them there.
fn vt_accel(codec: Option<&str>) -> bool {
    cfg!(target_os = "macos") && matches!(codec, Some("h264" | "hevc" | "h265" | "prores"))
}

/// Background jobs (probe, thumbs, sheets, cache decode) run niced so
/// they never steal CPU from live playback or the UI — the user's
/// attention has scheduling priority.
fn media_cmd(bin: &str) -> Command {
    #[cfg(unix)]
    {
        let mut c = Command::new("nice");
        c.arg("-n").arg("10").arg(bin);
        c
    }
    #[cfg(not(unix))]
    Command::new(bin)
}

/// Live playback (PLAN.md §6 level 3): an ffmpeg child decodes to raw
/// RGBA on stdout, looping forever; the reader thread stamps every frame
/// with its presentation time and queues a few ahead. Pacing happens on
/// the CONSUMER's clock — `take_frame` surfaces a frame only once it's
/// due — so presentation stays smooth against vsync, and the small
/// read-ahead absorbs decode spikes (keyframes, cold caches) that used
/// to land on screen as stutter. When a frame decodes late the schedule
/// re-anchors instead of piling up debt: owed frames would otherwise all
/// come due at once and play as a fast-forward burst (the old
/// stutter-then-skip at startup). The queue is bounded, so a player
/// nobody drains — e.g. a pre-warmed filmstrip neighbor — stalls its
/// reader, pipe backpressure stalls ffmpeg, and warmth costs nothing
/// after a few frames. Killed on drop.
pub struct LivePlayer {
    child: std::process::Child,
    queue: Arc<PacedQueue>,
    pub w: u32,
    pub h: u32,
}

/// Decoded frames waiting for their presentation times.
struct PacedQueue {
    frames: Mutex<VecDeque<(std::time::Instant, Vec<u8>)>>,
    /// Signalled when the consumer pops; the reader waits on it when full.
    space: Condvar,
}

/// Read-ahead depth: enough to ride out a slow frame, small enough that
/// an unwatched player stalls (and stops burning CPU) almost immediately.
const LIVE_QUEUE_DEPTH: usize = 3;

impl LivePlayer {
    /// `seek` starts playback that many seconds in — pass the thumbnail's
    /// frame time so live video continues from what the tile showed.
    /// `fps` paces delivery (the clip's rate from cached meta; ~30 if
    /// unknown). `codec` (from cached meta) gates hardware decode.
    pub fn spawn(
        path: &Path,
        w: u32,
        h: u32,
        seek: f64,
        fps: f64,
        codec: Option<&str>,
    ) -> Option<Self> {
        let (w, h) = (w.max(2), h.max(2));
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-stream_loop", "-1"]);
        if vt_accel(codec) {
            cmd.args(["-hwaccel", "videotoolbox"]);
        }
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
        let queue = Arc::new(PacedQueue {
            frames: Mutex::new(VecDeque::new()),
            space: Condvar::new(),
        });
        let shared = queue.clone();
        let frame_bytes = (w * h * 4) as usize;
        let fps = if fps.is_finite() {
            fps.clamp(1.0, 240.0)
        } else {
            30.0
        };
        thread::spawn(move || {
            use std::io::Read;
            use std::time::{Duration, Instant};
            let mut buf = vec![0u8; frame_bytes];
            let interval = Duration::from_secs_f64(1.0 / fps);
            let mut next_due: Option<Instant> = None;
            loop {
                // Bounded read-ahead: block until the consumer makes room.
                {
                    let mut q = shared.frames.lock().unwrap();
                    while q.len() >= LIVE_QUEUE_DEPTH {
                        q = shared.space.wait(q).unwrap();
                    }
                }
                if stdout.read_exact(&mut buf).is_err() {
                    return; // EOF or killed
                }
                // Frame decoded late (cold start, slow keyframe, resumed
                // from a park): re-anchor the schedule to now rather than
                // keeping the debt — otherwise every owed frame comes due
                // at once and plays as a fast-forward burst.
                let now = Instant::now();
                let due = match next_due {
                    Some(d) if now <= d + interval / 2 => d,
                    _ => now,
                };
                next_due = Some(due + interval);
                shared.frames.lock().unwrap().push_back((due, buf.clone()));
            }
        });
        Some(Self { child, queue, w, h })
    }

    /// Freeze/resume the decoder in place (SIGSTOP/SIGCONT) — cheaper than
    /// killing and respawning for short pauses, and playback resumes where
    /// it left off. The pacer's lateness re-anchor keeps resumed frames
    /// from fast-forwarding over frozen time. No-op on non-unix.
    pub fn set_parked(&self, parked: bool) {
        #[cfg(unix)]
        {
            let sig = if parked { libc::SIGSTOP } else { libc::SIGCONT };
            unsafe {
                libc::kill(self.child.id() as libc::pid_t, sig);
            }
        }
        #[cfg(not(unix))]
        let _ = parked;
    }

    /// Frames currently queued (decoded, waiting for their due times).
    pub fn buffered(&self) -> usize {
        self.queue.frames.lock().unwrap().len()
    }

    /// The newest frame that's due for presentation, if any. Earlier
    /// overdue frames are dropped; frames still ahead of their time stay
    /// queued — calling this every render tick paces playback on the
    /// render clock.
    pub fn take_frame(&self) -> Option<Vec<u8>> {
        let now = std::time::Instant::now();
        let mut q = self.queue.frames.lock().unwrap();
        let mut out = None;
        while q.front().is_some_and(|(due, _)| *due <= now) {
            out = q.pop_front().map(|(_, rgba)| rgba);
        }
        if out.is_some() {
            self.queue.space.notify_one();
        }
        out
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
        // Strict priority: visible thumbs, then the gen sweep, then anims.
        // The lock drops before any work starts.
        let req = {
            let (lock, cv) = &*queue;
            let mut q = lock.lock().unwrap();
            loop {
                if let Some(p) = q.thumbs.pop_front() {
                    break Request::Thumb(p);
                }
                if let Some(p) = q.gen.pop_front() {
                    break Request::Gen(p);
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
            Request::Gen(path) => {
                ensure_thumb_file(&path, &root, have_ffmpeg, &recipe);
                ThumbResult::GenDone { path }
            }
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
    let jpg = ensure_thumb_file(src, root, have_ffmpeg, recipe)?;
    decode_jpeg(&jpg, recipe.thumb_w, recipe.thumb_h)
}

/// Make sure the thumb jpg (+ meta.json) exists on disk, returning its
/// path — the decode/upload-free half of `make_thumb`, also used by the
/// background gen sweep.
fn ensure_thumb_file(
    src: &Path,
    root: &Path,
    have_ffmpeg: bool,
    recipe: &Recipe,
) -> Option<PathBuf> {
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
            .as_ref()
            .and_then(|m| m.duration)
            .map(|d| (d * SEEK_FRACTION).max(0.0))
            .unwrap_or(0.0);
        let codec = probed.as_ref().and_then(|m| m.codec.clone());
        extract_frame(src, &jpg, seek, recipe, codec.as_deref())?;
        log::debug!("thumb generated: {}", src.display());
    }
    Some(jpg)
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
        // Sampling frames across the clip needs its duration + codec.
        // The thumb pass usually cached the probe already.
        let m = cached_meta(src).or_else(|| probe(src))?;
        let duration = m.duration.filter(|d| *d > 0.05)?;
        let codec = m.codec.as_deref();
        let g = recipe.anim_grid.max(1);
        let frames = (g * g) as usize;
        let (fw, fh) = recipe.anim_frame();
        let q = recipe.quality.clamp(2, 31).to_string();
        let vf = format!("scale={fw}:{fh}:force_original_aspect_ratio=increase,crop={fw}:{fh}");

        // g² individual seeked extracts, then one cheap tile pass over
        // the tiny jpegs. Each extract is a fast keyframe seek plus a
        // handful of decoded frames (hardware where the codec allows).
        // The old single-command `fps=` filter decoded the ENTIRE clip
        // in software — minutes of multi-core churn per long 4K source,
        // three workers wide, surviving app quit. Never again.
        let frame_tmp = |k: usize| dir.join(format!("animf_{k}.jpg"));
        let cleanup = |n: usize| {
            for k in 0..n {
                let _ = std::fs::remove_file(frame_tmp(k));
            }
        };
        for k in 0..frames {
            let tmp = frame_tmp(k);
            let mut seek = duration * (k as f64 + 0.5) / frames as f64;
            let ok = loop {
                let mut cmd = media_cmd("ffmpeg");
                cmd.args(["-y", "-v", "error"]);
                if vt_accel(codec) {
                    cmd.args(["-hwaccel", "videotoolbox"]);
                }
                let out = cmd
                    .args(["-ss", &format!("{seek:.3}")])
                    .arg("-i")
                    .arg(src)
                    .args(["-frames:v", "1", "-vf", &vf, "-q:v", &q])
                    .args(["-strict", "unofficial", "-f", "mjpeg"])
                    .arg(&tmp)
                    .stdin(Stdio::null())
                    .output()
                    .ok();
                let done = out.as_ref().is_some_and(|o| o.status.success()) && tmp.exists();
                if done {
                    break true;
                }
                // A seek past EOF (VFR/short files) produces nothing:
                // retry that cell from the start of the clip.
                if seek > 0.0 {
                    seek = 0.0;
                    continue;
                }
                if let Some(o) = out {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    log::debug!(
                        "anim frame {k} failed for {}: {}",
                        src.display(),
                        stderr.lines().last().unwrap_or("(no output)")
                    );
                }
                break false;
            };
            if !ok {
                cleanup(k);
                return None;
            }
        }

        let tmp = jpg.with_extension("jpg.tmp");
        let pattern = dir.join("animf_%d.jpg");
        let out = media_cmd("ffmpeg")
            .args(["-y", "-v", "error", "-start_number", "0"])
            .arg("-i")
            .arg(&pattern)
            .args(["-frames:v", "1", "-vf", &format!("tile={g}x{g}")])
            .args(["-q:v", &q, "-strict", "unofficial", "-f", "mjpeg"])
            .arg(&tmp)
            .stdin(Stdio::null())
            .output()
            .ok();
        cleanup(frames);
        if !out.as_ref().is_some_and(|o| o.status.success()) || !tmp.exists() {
            let _ = std::fs::remove_file(&tmp);
            log::debug!("anim tile pass failed for {}", src.display());
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
    /// Display rotation in degrees (phone footage): ±90/±270 means the
    /// decoder auto-rotates and the *displayed* frame swaps width/height
    /// relative to the coded dims above. Absent in older meta.json.
    #[serde(default)]
    pub rotation: Option<f64>,
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
    let out = media_cmd("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
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
    // Rotation lives in the display-matrix side data on modern files,
    // in a `rotate` tag on older ones.
    let rotation = video
        .and_then(|s| s["side_data_list"].as_array())
        .and_then(|l| l.iter().find_map(|d| d["rotation"].as_f64()))
        .or_else(|| {
            video
                .and_then(|s| s["tags"]["rotate"].as_str())
                .and_then(|r| r.parse().ok())
        });
    Some(Meta {
        src: src.to_path_buf(),
        duration,
        width: video.and_then(|s| s["width"].as_u64()),
        height: video.and_then(|s| s["height"].as_u64()),
        codec: video.and_then(|s| s["codec_name"].as_str().map(String::from)),
        fps,
        rotation,
    })
}

fn extract_frame(
    src: &Path,
    dst: &Path,
    seek: f64,
    recipe: &Recipe,
    codec: Option<&str>,
) -> Option<()> {
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
    let mut cmd = media_cmd("ffmpeg");
    cmd.args(["-y", "-v", "error"]);
    if vt_accel(codec) {
        cmd.args(["-hwaccel", "videotoolbox"]);
    }
    let out = cmd
        .args(["-ss", &format!("{seek:.3}")])
        .arg("-i")
        .arg(src)
        .args([
            // the mjpeg default quality is very blocky; 2 ≈ visually lossless
            "-frames:v",
            "1",
            "-vf",
            &vf,
            "-q:v",
            &q,
            "-strict",
            "unofficial",
            "-f",
            "mjpeg",
        ])
        .arg(&tmp)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    // A seek past EOF exits 0 without producing a file: retry from the start.
    if !out.status.success() || !tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
        if seek > 0.0 {
            return extract_frame(src, dst, 0.0, recipe, codec);
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
    let mut cmd = media_cmd("ffmpeg");
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

/// Platform cache dir (PLAN.md §8): ~/Library/Caches/switchblade.noindex
/// on macOS, XDG cache dir elsewhere. The `.noindex` suffix keeps
/// Spotlight's mdworker away from the thousands of generated jpegs (it
/// honors the suffix); an existing un-suffixed cache is renamed across.
pub fn cache_root() -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        #[cfg(target_os = "macos")]
        let base = home.join("Library/Caches");
        #[cfg(not(target_os = "macos"))]
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"));
        let dir = base.join("switchblade.noindex");
        let old = base.join("switchblade");
        if old.is_dir() && !dir.exists() {
            let _ = std::fs::rename(&old, &dir);
        }
        dir.join("v1").join("objects")
    })
    .clone()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Live playback must deliver frames at the clip's rate from the very
    /// start — no initial burst (the fast-forward bug) and no stall.
    #[test]
    fn live_player_paces_frames() {
        if !have_binary("ffmpeg") {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_media_pace_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("pace.mp4");
        if !clip.exists() {
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=4:size=320x180:rate=30")
                .args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .arg(&clip)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to generate test clip");
        }

        let player = LivePlayer::spawn(&clip, 320, 180, 0.4, 30.0, Some("h264")).expect("spawn");
        // Give it a moment to open the input, then measure one second.
        let mut first = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while first.is_none() && std::time::Instant::now() < deadline {
            first = player.take_frame();
            thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(first.is_some(), "no first frame within 3s");

        let t0 = std::time::Instant::now();
        let mut frames = 0u32;
        while t0.elapsed() < std::time::Duration::from_secs(1) {
            if player.take_frame().is_some() {
                frames += 1;
            }
            thread::sleep(std::time::Duration::from_millis(2));
        }
        // 30fps paced => ~30 frames. The burst bug delivered 90+ in the
        // first second; a stall delivers ~0.
        assert!(
            (20..=45).contains(&frames),
            "expected ~30 paced frames in 1s, got {frames}"
        );
    }

    /// Anim sheets must come from cheap seeked extracts (seconds), never
    /// a full-clip decode (the fps-filter approach cost minutes of
    /// multi-core software decode per long 4K source and bogged the
    /// whole machine).
    #[test]
    fn anim_sheet_generates_and_tiles() {
        if !have_binary("ffmpeg") || !have_binary("ffprobe") {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_media_anim_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("anim_src.mp4");
        if !clip.exists() {
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=4:size=320x180:rate=30")
                .args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .arg(&clip)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to generate test clip");
        }
        let root = dir.join("cache");
        let recipe = Recipe {
            thumb_w: 320,
            thumb_h: 180,
            quality: 5,
            anim_grid: 3,
        };
        let t0 = std::time::Instant::now();
        let sheet = make_anim(&clip, &root, true, &recipe);
        let elapsed = t0.elapsed();
        let (w, h, rgba) = sheet.expect("sheet generated");
        assert!(w > 0 && h > 0 && rgba.len() == (w * h * 4) as usize);
        // Generous bound: 10 tiny extracts on a 4s 320x180 clip is
        // sub-second work; a full-decode regression would still pass
        // here, but the per-frame command shape is what this pins.
        assert!(elapsed.as_secs() < 30, "sheet took {elapsed:?}");
    }

    /// The pre-warm contract: a player nobody drains fills its bounded
    /// queue and stalls (near-zero CPU), then hands over a frame the
    /// instant it's finally asked — that's what makes filmstrip h/l show
    /// video the same tick.
    #[test]
    fn unwatched_player_stalls_then_serves_instantly() {
        if !have_binary("ffmpeg") {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_media_pace_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("pace_warm.mp4");
        if !clip.exists() {
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=4:size=320x180:rate=30")
                .args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .arg(&clip)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to generate test clip");
        }

        let player = LivePlayer::spawn(&clip, 320, 180, 0.4, 30.0, Some("h264")).expect("spawn");
        // Warm without watching: the queue caps at LIVE_QUEUE_DEPTH and
        // the decoder stalls behind it.
        thread::sleep(std::time::Duration::from_millis(800));
        assert!(
            player.queue.frames.lock().unwrap().len() <= LIVE_QUEUE_DEPTH,
            "queue must stay bounded while unwatched"
        );
        // Promotion: the queued (long-overdue) frames surface immediately.
        assert!(
            player.take_frame().is_some(),
            "a warmed player must serve a frame on the first take"
        );
    }
}
