//! sb-media: media probing, thumbnail extraction, and the filesystem cache.
//!
//! PLAN.md §6 (media levels), §7 (sidecar cache), §8 (filesystem-first),
//! §15 (media backend spike: start with external ffmpeg/ffprobe).
//!
//! A small worker pool extracts one representative frame per clip via the
//! `ffmpeg` CLI into a content-addressed sidecar cache, then decodes it to
//! RGBA for the renderer's atlas. The render thread never blocks on this.

pub mod maintenance;
pub mod probe;
mod seekable;
pub use probe::{CounterSnapshot, EventKind, Lane, LaneProbe, Probe, RelEvent};
pub use seekable::SeekablePlayer;
/// Re-exported so sb-app's bench runner can serialize its reports without
/// taking its own serde_json dependency (it's already built for us here).
pub use serde_json;

use std::collections::{HashMap, VecDeque};
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
    /// Anim sheets are `anim_grid × anim_grid` true-aspect frames sampled
    /// evenly across the clip (PLAN.md §6 level 2), each fit inside a
    /// thumb/anim_grid cell box, packed into one slot. 3 = more motion,
    /// 2 = crisper.
    pub anim_grid: u32,
    /// Fraction of the clip's duration the STATIC thumb is extracted at
    /// (0..1). Live playback seeks to the same fraction so it continues
    /// from the frame the tile already shows — so this value governs both,
    /// and it's part of the artifact name below so a change regenerates
    /// the thumb (a stale-fraction thumb would jolt when live starts).
    pub seek_fraction: f32,
}

impl Recipe {
    /// Filename suffix for a non-default seek fraction (e.g. `_s25` = 25%).
    /// The historical default (`SEEK_FRACTION`) keeps the original
    /// suffix-less name so existing caches aren't invalidated on upgrade;
    /// only opting into a different fraction regenerates.
    fn seek_suffix(&self) -> String {
        if (self.seek_fraction as f64 - SEEK_FRACTION).abs() < 0.0005 {
            String::new()
        } else {
            format!(
                "_s{}",
                (self.seek_fraction.clamp(0.0, 1.0) * 100.0).round() as u32
            )
        }
    }
    fn thumb_file(&self) -> String {
        format!(
            "thumb_fit_{}x{}_q{}{}.jpg",
            self.thumb_w,
            self.thumb_h,
            self.quality,
            self.seek_suffix()
        )
    }
    fn anim_file(&self) -> String {
        // `_fit`: cells preserve the source aspect (fit into the cell box),
        // not the legacy 16:9 crop-fill — the suffix regenerates old
        // cropped sheets rather than serving them.
        let g = self.anim_grid;
        format!(
            "anim_{g}x{g}_{}x{}_q{}_fit.jpg",
            self.thumb_w, self.thumb_h, self.quality
        )
    }
    fn anim_frame(&self) -> (u32, u32) {
        let g = self.anim_grid.max(1);
        ((self.thumb_w / g).max(2), (self.thumb_h / g).max(2))
    }
}

const WORKERS: usize = 3;

/// Fired after each result lands in the channel, so the render loop can
/// wake for it instead of polling (PERFORMANCE-TASKS.md P0.2). The app
/// passes its window-layer wake handle wrapped in a closure.
pub type Notify = Arc<dyn Fn() + Send + Sync>;
/// Default fraction into the clip the thumb is extracted at (PLAN.md §6
/// initial policy) — the `thumb_seek_fraction` tuning overrides it via
/// `Recipe::seek_fraction`. Kept public as the default and so callers
/// without a recipe (maintenance) share one constant.
pub const SEEK_FRACTION: f64 = 0.10;

enum Request {
    Thumb(PathBuf),
    /// Re-run ffprobe and rewrite meta.json — heals cache entries
    /// written before a `Meta` field existed. No result is sent.
    Reprobe(PathBuf),
    /// Chapter probe for the fullview chapter bar: one ffprobe
    /// `-show_chapters` run answering "does this file have chapters, and
    /// where do they start?" (plus the container duration for the app's
    /// synthesized-checkpoint fallback). The bar's chip IMAGES come from
    /// the clip's cached anim sheet — no extraction happens here.
    Chapters(PathBuf),
    /// Generate the thumb (+ meta) on disk without decoding/uploading —
    /// the library-wide background sweep.
    Gen(PathBuf),
    Anim(PathBuf),
}

/// Which cached artifact a job produces — the coalescing key (P1.2):
/// `Thumb` covers both the visible-thumb request and the gen sweep
/// (identical jpg + meta on disk).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Art {
    Thumb,
    Anim,
}

/// Results owed to requests that merged into someone else's generation
/// (P1.2). The app counts one result per request, so a merged request
/// still gets its result — just without a second ffprobe/ffmpeg run.
#[derive(Default)]
struct Owed {
    /// Extra `GenDone`s (gen requests absorbed by a thumb job or a
    /// duplicate gen).
    gens: u32,
    /// Foreground decode+`Ready`s (visible requests that joined an
    /// in-flight generation).
    thumbs: u32,
    /// Extra anim results (duplicate on-demand sheet requests).
    anims: u32,
}

/// Strict-priority work queue, popped top to bottom — a lower tier never
/// runs while a higher one has work:
///   1. `thumbs`    — visible thumbnails (something on screen needs pixels)
///   2. `reprobes`  — meta.json healing for clips the user is playing
///      live right now (one cheap ffprobe unblocks the hardware scale
///      chain for that clip's next spawn)
///   3. `anims_now` — the ONE sheet quickview/fullview asked for on
///      demand (seekbar storyboard, chapter chips): the user is looking
///      at it right now, so it must not wait behind the sweep — after a
///      thumb-recipe change the `gen` tier holds the whole library for
///      hours across sessions, and tier-strict starvation below it
///      silently killed the storyboard. This is the ONLY sheet tier —
///      the old library-wide bulk sweep is gone, so sheets exist only
///      for clips the user actually opened.
///   4. `gen`       — background thumb generation for the whole library
///
/// (Live video never queues here at all — it runs in-process; these
/// workers are niced below it.)
#[derive(Default)]
struct Queues {
    thumbs: VecDeque<PathBuf>,
    reprobes: VecDeque<PathBuf>,
    /// Chapter-probe requests (tier 3, beside `anims_now`: the user is
    /// in fullview, where the bar can open any moment). Deduplicated by
    /// path — fullview pre-warms a probe per visited clip and they're
    /// each one cheap ffprobe.
    chapters: VecDeque<PathBuf>,
    anims_now: VecDeque<PathBuf>,
    r#gen: VecDeque<PathBuf>,
    // ── artifact coalescing (P1.2) ──────────────────────────────────
    // Membership mirrors of the four artifact queues (reprobes are
    // self-deduped app-side and cheap): O(1) duplicate checks — the
    // linear VecDeque scan only runs on an actual promotion/absorption.
    in_thumbs: std::collections::HashSet<PathBuf>,
    in_gen: std::collections::HashSet<PathBuf>,
    in_anims_now: std::collections::HashSet<PathBuf>,
    /// Artifacts a worker is generating right now.
    inflight: std::collections::HashSet<(PathBuf, Art)>,
    /// Extra results owed per path when its artifact completes.
    gen_owed: HashMap<PathBuf, u32>,
    thumb_owed: HashMap<PathBuf, u32>,
    anim_owed: HashMap<PathBuf, u32>,
    /// The selected stream is presenting frames right now (mirrors the
    /// app's `MEDIA_UPLOAD_BUDGET_LIVE` condition). While set, the gen
    /// sweep narrows to `gen_live_cap` concurrent jobs: several workers'
    /// worth of seeked 4K extracts contend for CPU + the VT media engine
    /// during a stream's cold spawn — watching wins.
    live: bool,
    /// Gen-sweep jobs currently running (the counter the live cap gates).
    gen_running: usize,
    /// Concurrent gen-sweep jobs allowed while `live` (config
    /// `gen_live_concurrency`, default 1). `0` = unlimited (never throttle
    /// the sweep). Set from `Tuning` at `MediaService::new`; `Queues::default`
    /// leaves it 0 so unit tests opt into the cap explicitly.
    gen_live_cap: usize,
}

impl Queues {
    fn thumb_busy(&self, p: &Path) -> bool {
        self.inflight.contains(&(p.to_path_buf(), Art::Thumb))
    }
    fn anim_busy(&self, p: &Path) -> bool {
        self.inflight.contains(&(p.to_path_buf(), Art::Anim))
    }

    /// Visible-thumb request (tier 1). A gen-sweep entry for the same
    /// artifact is absorbed (promoted to this tier); joining queued or
    /// in-flight work records an owed foreground result instead of a
    /// duplicate generation.
    fn push_thumb(&mut self, p: PathBuf) {
        if self.in_gen.remove(&p) {
            let i = self.r#gen.iter().position(|q| q == &p).unwrap();
            self.r#gen.remove(i);
            *self.gen_owed.entry(p.clone()).or_default() += 1;
        }
        if self.in_thumbs.contains(&p) || self.thumb_busy(&p) {
            *self.thumb_owed.entry(p).or_default() += 1;
        } else {
            self.in_thumbs.insert(p.clone());
            self.thumbs.push_back(p);
        }
    }

    /// Gen-sweep request (tier 4). The same artifact already queued or
    /// running (either kind of thumb work) just owes one more `GenDone`.
    fn push_gen(&mut self, p: PathBuf) {
        if self.in_thumbs.contains(&p) || self.in_gen.contains(&p) || self.thumb_busy(&p) {
            *self.gen_owed.entry(p).or_default() += 1;
        } else {
            self.in_gen.insert(p.clone());
            self.r#gen.push_back(p);
        }
    }

    /// The on-demand sheet (tier 3). A duplicate request for a sheet
    /// already queued or generating owes one more result instead of a
    /// second generation (P1.2).
    fn push_anim_now(&mut self, p: PathBuf) {
        if self.in_anims_now.contains(&p) || self.anim_busy(&p) {
            *self.anim_owed.entry(p).or_default() += 1;
        } else {
            self.in_anims_now.insert(p.clone());
            self.anims_now.push_back(p);
        }
    }

    fn pop(&mut self) -> Option<Request> {
        if let Some(p) = self.thumbs.pop_front() {
            self.in_thumbs.remove(&p);
            self.inflight.insert((p.clone(), Art::Thumb));
            return Some(Request::Thumb(p));
        }
        if let Some(p) = self.reprobes.pop_front() {
            return Some(Request::Reprobe(p));
        }
        if let Some(p) = self.chapters.pop_front() {
            return Some(Request::Chapters(p));
        }
        if let Some(p) = self.anims_now.pop_front() {
            self.in_anims_now.remove(&p);
            self.inflight.insert((p.clone(), Art::Anim));
            return Some(Request::Anim(p));
        }
        // Live cap: leave gen work queued rather than racing the
        // presenting stream for the drive. Workers park on the condvar;
        // `set_live(false)` and each finishing gen/anim job re-notify. A
        // `gen_live_cap` of 0 disables the cap (never throttle the sweep).
        //
        // An on-demand storyboard sheet queued or generating (`anims_now`
        // tier) is user-attention work the user is actively waiting on — the
        // chapter bar / seekbar preview won't draw until it lands. Pause the
        // gen sweep ENTIRELY while one is in flight so the sheet's nine
        // sw-decoded 4K extracts don't fight a concurrent 4K sweep decode for
        // CPU/memory bandwidth (benchmarks/reports/chapter_sheet_latency.md:
        // that contention stretched a ~3s sheet to ~23s). Independent of
        // `live` — the stalling stream can read as not-live exactly when this
        // matters most.
        let anim_in_flight = !self.anims_now.is_empty()
            || self.inflight.iter().any(|(_, a)| matches!(a, Art::Anim));
        let capped = anim_in_flight
            || (self.live && self.gen_live_cap != 0 && self.gen_running >= self.gen_live_cap);
        if !capped
            && let Some(p) = self.r#gen.pop_front()
        {
            self.in_gen.remove(&p);
            self.inflight.insert((p.clone(), Art::Thumb));
            self.gen_running += 1;
            return Some(Request::Gen(p));
        }
        None
    }

    /// A worker finished `p`'s `art` job: clear in-flight and collect
    /// whatever merged requests are owed. Requests arriving after this
    /// enqueue normally — the artifact is cached now, so they're cheap.
    fn complete(&mut self, p: &Path, art: Art) -> Owed {
        self.inflight.remove(&(p.to_path_buf(), art));
        match art {
            Art::Thumb => Owed {
                gens: self.gen_owed.remove(p).unwrap_or(0),
                thumbs: self.thumb_owed.remove(p).unwrap_or(0),
                anims: 0,
            },
            Art::Anim => Owed {
                gens: 0,
                thumbs: 0,
                anims: self.anim_owed.remove(p).unwrap_or(0),
            },
        }
    }
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
    /// A sprite sheet of ANIM_FRAMES true-aspect frames; `w × h` are the
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
    /// The chapter probe's answer: the file's chapter start times
    /// (ascending; empty when it has none or the probe failed) plus the
    /// container duration, for the app's synthesized-checkpoint
    /// fallback. Chip images come from the clip's anim sheet — nothing
    /// else follows.
    ChapterTimes {
        path: PathBuf,
        times: Vec<f64>,
        duration: Option<f64>,
    },
}

/// Async thumbnail service. `request` from the UI thread, results arrive
/// via `try_recv` on later frames.
pub struct MediaService {
    queue: SharedQueue,
    rx: Receiver<ThumbResult>,
    recipe: Recipe,
    /// Last value passed to `set_live` — dedupes the app's per-frame
    /// call down to a relaxed atomic load on the common no-change path.
    live: std::sync::atomic::AtomicBool,
}

impl MediaService {
    /// `gen_live_cap` = concurrent gen-sweep jobs allowed while the selected
    /// stream presents (config `gen_live_concurrency`; 0 = unlimited).
    pub fn new(recipe: Recipe, notify: Notify, gen_live_cap: usize) -> Self {
        // Start with the gen throttle ENGAGED. The workers begin draining the
        // queue during ingest, BEFORE the app's first `set_live` call — so an
        // uncapped default let the sweep grab all workers with slow 4K gen
        // extracts at startup, starving the watched stream's cold spawn (first
        // frame delayed to ~5s) and the on-demand storyboard behind it. The
        // app opens the sweep to full width on the first frame where nothing
        // is being watched (`set_live(false)`), so a no-video session pays at
        // most one frame of throttling.
        let queue: SharedQueue = Arc::new((
            Mutex::new(Queues {
                gen_live_cap,
                live: true,
                ..Default::default()
            }),
            Condvar::new(),
        ));
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
            let notify = notify.clone();
            thread::spawn(move || worker(q, tx, notify, root, have_ffmpeg, recipe));
        }
        Self {
            queue,
            rx: rx_done,
            recipe,
            // Kept in sync with the queue's `live` above so the app's first
            // `set_live(false)` (no video) registers as a real change and
            // opens the sweep; a first `set_live(true)` is a correct no-op.
            live: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Tell the workers whether the selected stream is presenting frames
    /// (the app's `MEDIA_UPLOAD_BUDGET_LIVE` condition). While true the
    /// gen sweep narrows to `gen_live_cap` jobs so its drive
    /// reads can't starve the resident decoder; on the falling edge every
    /// parked worker wakes and the sweep resumes at full width. Cheap to
    /// call every frame — no-ops without a state change.
    pub fn set_live(&self, live: bool) {
        use std::sync::atomic::Ordering;
        if self.live.swap(live, Ordering::Relaxed) == live {
            return;
        }
        let (lock, cv) = &*self.queue;
        lock.lock().unwrap().live = live;
        if !live {
            cv.notify_all();
        }
    }

    /// Disk path of the clip's cached static thumbnail under the current
    /// recipe, if it exists — used as the drag-out ghost image. Cheap
    /// (one stat + one exists check); never generates or queues anything.
    pub fn cached_thumb_path(&self, path: &Path) -> Option<PathBuf> {
        let st = std::fs::metadata(path).ok()?;
        let file = entry_dir(&cache_root(), path, st.len(), mtime_secs(&st))
            .join(self.recipe.thumb_file());
        file.exists().then_some(file)
    }

    pub fn request(&self, path: PathBuf) {
        let (lock, cv) = &*self.queue;
        lock.lock().unwrap().push_thumb(path);
        cv.notify_one();
    }

    /// Queue a background meta.json rewrite for a clip whose cached
    /// probe predates a `Meta` field (today: `pix_fmt`, which gates the
    /// hardware scale chain — the clip plays through the software chain
    /// until healed). Duplicate requests are cheap no-ops (the worker
    /// skips entries that are already complete), but callers should
    /// still dedup per session to keep the queue clean.
    pub fn request_reprobe(&self, path: PathBuf) {
        let (lock, cv) = &*self.queue;
        lock.lock().unwrap().reprobes.push_back(path);
        cv.notify_one();
    }

    /// Queue a chapter probe (one cheap ffprobe answering whether the
    /// file has chapters and where they start). Deduped by path; every
    /// queued probe gets its answer — the app caches them per clip, so
    /// fullview can pre-warm ahead of the bar opening.
    pub fn request_chapters(&self, path: PathBuf) {
        let (lock, cv) = &*self.queue;
        {
            let mut q = lock.lock().unwrap();
            if !q.chapters.contains(&path) {
                q.chapters.push_back(path);
            }
        }
        cv.notify_one();
    }

    /// Queue background thumb generation (disk cache only, no upload).
    pub fn request_gen(&self, path: PathBuf) {
        let (lock, cv) = &*self.queue;
        lock.lock().unwrap().push_gen(path);
        cv.notify_one();
    }

    /// Queue one clip's anim sheet, above the background gen sweep — the
    /// only way sheets are made: quickview/fullview request the selected
    /// clip's sheet on demand (seekbar storyboard, chapter chips). There
    /// is deliberately no library-wide sheet sweep — clips the user never
    /// opens never pay for one.
    pub fn request_anim_now(&self, path: PathBuf) {
        let (lock, cv) = &*self.queue;
        lock.lock().unwrap().push_anim_now(path);
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

/// Hardware *scaling* for live playback: keep frames on the GPU through
/// decode → (transpose_vt) → scale_vt, and only `hwdownload` at TARGET
/// resolution — the CPU then converts ~1.5Mpx to RGBA instead of
/// scaling 8Mpx down first. Measured on 4K60 HEVC → 1440p: 2.3× the
/// throughput at ~2.5× less CPU than the software chain (PERF.md §1).
///
/// Returns the `-vf` string, or None when the clip must take the
/// software chain instead:
/// - non-VT codec (VP9/AV1 decode *slower* through VT — see `vt_accel`);
/// - unknown/exotic pixel format: `hwdownload` requires the raw format
///   named explicitly, and it differs by bit depth (nv12 for 8-bit
///   4:2:0, p010le for 10-bit; 4:2:2/4:4:4 aren't worth a hw mapping);
///   callers must pass dims already aligned down to a multiple of 8
///   (see `spawn`) or delivery jitters;
/// - a rotation hw frames can't express: they do NOT autorotate
///   (verified — misrotated output scores ~0.25dB PSNR vs ground
///   truth). ±90° maps onto transpose_vt (direction PSNR-verified,
///   31dB+ both signs); 180° and odd angles fall back to software,
///   which autorotates correctly.
fn hw_scale_vf(
    codec: Option<&str>,
    pix_fmt: Option<&str>,
    rotation: Option<f64>,
    w: u32,
    h: u32,
) -> Option<String> {
    if !vt_accel(codec) {
        return None;
    }
    let dl = match pix_fmt? {
        "yuv420p" | "yuvj420p" | "nv12" => "nv12",
        "yuv420p10le" | "p010le" => "p010le",
        _ => return None,
    };
    let transpose = match rotation {
        None => "",
        Some(r) => {
            let q = (r / 90.0).round();
            if (r - q * 90.0).abs() > 1.0 {
                return None; // odd angle
            }
            match (q as i64).rem_euclid(4) {
                0 => "",
                1 => "transpose_vt=cclock,", // rotation +90 / -270
                3 => "transpose_vt=clock,",  // rotation -90 / +270
                _ => return None,            // 180°
            }
        }
    };
    Some(format!(
        "{transpose}scale_vt={w}:{h},hwdownload,format={dl},format=rgba"
    ))
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

/// Background-BAND variant for the sweep's heavy readers (gen extracts,
/// anim-sheet cells): `taskpolicy -b` puts the child in Darwin's
/// background band, which throttles its DISK I/O below normal-priority
/// reads — the priority `nice` cannot express. That's what protects a
/// live stream on a slow/external drive: the sweep's seeked extracts of
/// 4K sources otherwise saturate the drive and the resident decoder's
/// reads stall for seconds (observed as multi-second `live 0` droughts
/// on an encrypted USB volume). Foreground work (visible thumbs, cache
/// jpeg decode) stays on plain `media_cmd` — throttled I/O would add
/// deliberate sleeps to user-facing loads.
fn media_cmd_bg(bin: &str) -> Command {
    #[cfg(target_os = "macos")]
    if std::path::Path::new("/usr/sbin/taskpolicy").exists() {
        let mut c = Command::new("/usr/sbin/taskpolicy");
        c.arg("-b").arg(bin);
        return c;
    }
    media_cmd(bin)
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
    /// Raised on drop. A stalled player's reader is parked on `space`
    /// with a FULL queue (that's every warm player's steady state) — the
    /// child's death can't wake it there, and without this flag the
    /// thread would block forever, pinning ~30MB of frame buffers per
    /// dropped player. That leak compounded into the "live video
    /// throttles after browsing a while" bug.
    closed: std::sync::atomic::AtomicBool,
}

/// Read-ahead depth: enough to ride out a slow frame, small enough that
/// an unwatched player stalls (and stops burning CPU) almost immediately.
const LIVE_QUEUE_DEPTH: usize = 3;

impl LivePlayer {
    /// `seek` starts playback that many seconds in — pass the thumbnail's
    /// frame time so live video continues from what the tile showed.
    /// `meta` (the clip's cached probe) supplies fps for pacing (~30 if
    /// unknown), codec to gate hardware decode, and pix_fmt/rotation to
    /// gate hardware *scaling*. The actual decode size is `self.w/h` —
    /// the hardware chain rounds the request DOWN to a multiple of 8:
    /// unaligned scale_vt/hwdownload dims deliver frames with periodic
    /// ~2× interval jitter (measured: 8-bit needs mod-4, 10-bit mod-8;
    /// mod-2 gapped 4/s). Content is squeezed ≤7px, never cropped.
    pub fn spawn(path: &Path, w: u32, h: u32, seek: f64, meta: Option<&Meta>) -> Option<Self> {
        let (mut w, mut h) = (w.max(2), h.max(2));
        let fps = meta.and_then(|m| m.fps).unwrap_or(30.0);
        let codec = meta.and_then(|m| m.codec.as_deref());
        let hw_vf = if w >= 8 && h >= 8 {
            hw_scale_vf(
                codec,
                meta.and_then(|m| m.pix_fmt.as_deref()),
                meta.and_then(|m| m.rotation),
                w & !7,
                h & !7,
            )
        } else {
            None // tiny target: sw scaling is cheap and exact
        };
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-stream_loop", "-1"]);
        if hw_vf.is_some() {
            (w, h) = (w & !7, h & !7);
            // -noautorotate pins rotation handling to our explicit
            // transpose_vt: hw frames never autorotate today, but don't
            // let a future ffmpeg change that behind our back.
            cmd.args(["-noautorotate", "-hwaccel", "videotoolbox"]);
            cmd.args(["-hwaccel_output_format", "videotoolbox_vld"]);
        } else if vt_accel(codec) {
            cmd.args(["-hwaccel", "videotoolbox"]);
        }
        if seek > 0.05 {
            // Keyframe start (`-noaccurate_seek`), matching the thumbnail
            // and SeekablePlayer: the first frame is the keyframe ≤ seek,
            // no decode-forward — same frame the static thumb shows.
            cmd.args(["-ss", &format!("{seek:.3}"), "-noaccurate_seek"]);
        }
        let fps = if fps.is_finite() {
            fps.clamp(1.0, 240.0)
        } else {
            30.0
        };
        // Software fallback scales with fast_bilinear: at video rates the
        // frame persists ~17ms and the hires texture is mip-sampled, so
        // bicubic buys nothing visible — it cost ~0.7 core extra and the
        // difference between keeping up with 4K60 and not (100 vs 67fps
        // measured; thumb/anim generation keeps bicubic, quality matters
        // there and rate doesn't).
        let vf = hw_vf.unwrap_or_else(|| format!("scale={w}:{h}:flags=fast_bilinear"));
        // Output `-r` forces CFR at the pacing rate. The reader stamps
        // frames 1/fps apart and ASSUMES ffmpeg emits that cadence —
        // `-r` makes the assumption true by construction, so wrong or
        // missing fps meta and genuinely-VFR sources degrade to
        // dup/dropped frames at the right wall-clock speed instead of
        // playing at the wrong speed.
        let mut child = cmd
            .arg("-i")
            .arg(path)
            .args([
                "-an",
                "-sn",
                "-vf",
                &vf,
                "-r",
                &format!("{fps}"),
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
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        let shared = queue.clone();
        let frame_bytes = (w * h * 4) as usize;
        thread::spawn(move || {
            use std::io::Read;
            use std::sync::atomic::Ordering;
            use std::time::{Duration, Instant};
            let mut buf = vec![0u8; frame_bytes];
            let interval = Duration::from_secs_f64(1.0 / fps);
            let mut next_due: Option<Instant> = None;
            loop {
                // Bounded read-ahead: block until the consumer makes room
                // — or the player is dropped (`closed` + notify_all).
                {
                    let mut q = shared.frames.lock().unwrap();
                    while q.len() >= LIVE_QUEUE_DEPTH {
                        if shared.closed.load(Ordering::Relaxed) {
                            return;
                        }
                        q = shared.space.wait(q).unwrap();
                    }
                }
                if shared.closed.load(Ordering::Relaxed) {
                    return;
                }
                if stdout.read_exact(&mut buf).is_err() {
                    return; // EOF or killed
                }
                // Frame decoded late (cold start, slow keyframe, long
                // backpressure stall): re-anchor the schedule to now
                // rather than keeping the debt — otherwise every owed
                // frame comes due at once and plays as a fast-forward
                // burst.
                let now = Instant::now();
                let due = match next_due {
                    Some(d) if now <= d + interval / 2 => d,
                    _ => now,
                };
                next_due = Some(due + interval);
                // Hand the filled buffer over and allocate a fresh one
                // OUTSIDE the lock — `buf.clone()` as the push_back arg
                // ran a 14.7MB memcpy (1–2ms at 1440p) while holding the
                // mutex `take_frame` contends on every render tick.
                let frame = std::mem::replace(&mut buf, vec![0u8; frame_bytes]);
                shared.frames.lock().unwrap().push_back((due, frame));
            }
        });
        Some(Self { child, queue, w, h })
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
        // Wake a reader parked on the full-queue condvar so it exits
        // (killing the child only unblocks a reader stuck in read_exact).
        self.queue
            .closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.queue.space.notify_all();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn worker(
    queue: SharedQueue,
    tx: Sender<ThumbResult>,
    notify: Notify,
    root: PathBuf,
    have_ffmpeg: bool,
    recipe: Recipe,
) {
    loop {
        // Strict priority (tier order documented on `Queues`). The lock
        // drops before any work starts.
        let req = {
            let (lock, cv) = &*queue;
            let mut q = lock.lock().unwrap();
            loop {
                if let Some(r) = q.pop() {
                    break r;
                }
                q = cv.wait(q).unwrap();
            }
        };
        // One generation can settle several merged requests (P1.2): the
        // job's own result plus whatever `complete()` says is owed to
        // requests that joined instead of duplicating the work.
        let mut results: Vec<ThumbResult> = Vec::new();
        match req {
            Request::Thumb(path) => {
                let out = make_thumb(&path, &root, have_ffmpeg, &recipe);
                let owed = queue.0.lock().unwrap().complete(&path, Art::Thumb);
                for _ in 0..owed.gens {
                    results.push(ThumbResult::GenDone { path: path.clone() });
                }
                match out {
                    Some((w, h, rgba)) => {
                        for _ in 0..owed.thumbs {
                            results.push(ThumbResult::Ready {
                                path: path.clone(),
                                w,
                                h,
                                rgba: rgba.clone(),
                            });
                        }
                        results.push(ThumbResult::Ready { path, w, h, rgba });
                    }
                    None => {
                        for _ in 0..=owed.thumbs {
                            results.push(ThumbResult::Failed { path: path.clone() });
                        }
                    }
                }
            }
            Request::Reprobe(path) => {
                if have_ffmpeg {
                    reprobe(&path, &root);
                }
                continue; // nothing to upload, no result
            }
            Request::Chapters(path) => {
                let (times, duration) = if have_ffmpeg {
                    probe_chapter_info(&path).unwrap_or_default()
                } else {
                    Default::default()
                };
                results.push(ThumbResult::ChapterTimes {
                    path,
                    times,
                    duration,
                });
            }
            Request::Gen(path) => {
                // Background sweep: software decode (hw=false) keeps this
                // niced, full-library work off the VT media engine the
                // live stream needs.
                let jpg = ensure_thumb_file(&path, &root, have_ffmpeg, &recipe, false);
                let owed = {
                    let mut q = queue.0.lock().unwrap();
                    q.gen_running -= 1;
                    q.complete(&path, Art::Thumb)
                };
                // The freed live-cap slot may unblock a parked worker.
                queue.1.notify_one();
                // Visible requests that joined this generation get their
                // foreground decode now — the file just landed.
                if owed.thumbs > 0 {
                    let decoded = jpg
                        .as_deref()
                        .and_then(|j| decode_jpeg(j, recipe.thumb_w, recipe.thumb_h));
                    for _ in 0..owed.thumbs {
                        results.push(match &decoded {
                            Some((w, h, rgba)) => ThumbResult::Ready {
                                path: path.clone(),
                                w: *w,
                                h: *h,
                                rgba: rgba.clone(),
                            },
                            None => ThumbResult::Failed { path: path.clone() },
                        });
                    }
                }
                for _ in 0..owed.gens {
                    results.push(ThumbResult::GenDone { path: path.clone() });
                }
                results.push(ThumbResult::GenDone { path });
            }
            Request::Anim(path) => {
                let out = make_anim(&path, &root, have_ffmpeg, &recipe);
                let owed = queue.0.lock().unwrap().complete(&path, Art::Anim);
                // The sheet is done — gen was paused while it generated, so
                // wake a parked worker to resume the sweep.
                queue.1.notify_one();
                match out {
                    Some((w, h, rgba)) => {
                        for _ in 0..owed.anims {
                            results.push(ThumbResult::AnimReady {
                                path: path.clone(),
                                w,
                                h,
                                rgba: rgba.clone(),
                            });
                        }
                        results.push(ThumbResult::AnimReady { path, w, h, rgba });
                    }
                    None => {
                        for _ in 0..=owed.anims {
                            results.push(ThumbResult::AnimFailed { path: path.clone() });
                        }
                    }
                }
            }
        }
        for r in results {
            if tx.send(r).is_err() {
                return;
            }
        }
        notify(); // nudge the render loop to drain this batch
    }
}

/// Serve from the sidecar cache, generating on miss. See PLAN.md §7/§8.
fn make_thumb(
    src: &Path,
    root: &Path,
    have_ffmpeg: bool,
    recipe: &Recipe,
) -> Option<(u32, u32, Vec<u8>)> {
    // Foreground (a visible tile is waiting on it): VT keeps grid-fill snappy.
    let jpg = ensure_thumb_file(src, root, have_ffmpeg, recipe, true)?;
    decode_jpeg(&jpg, recipe.thumb_w, recipe.thumb_h)
}

/// Make sure the thumb jpg (+ meta.json) exists on disk, returning its
/// path — the decode/upload-free half of `make_thumb`, also used by the
/// background gen sweep. `hw` gates VideoToolbox: the sweep passes false
/// so its niced decodes never contend with live playback for the media
/// engine (see `extract_frame`).
fn ensure_thumb_file(
    src: &Path,
    root: &Path,
    have_ffmpeg: bool,
    recipe: &Recipe,
    hw: bool,
) -> Option<PathBuf> {
    let meta = std::fs::metadata(src).ok()?;
    if !meta.is_file() {
        return None;
    }
    let dir = entry_dir(root, src, meta.len(), mtime_secs(&meta));
    let jpg = dir.join(recipe.thumb_file());

    if !jpg.exists() {
        if !have_ffmpeg {
            return None;
        }
        std::fs::create_dir_all(&dir).ok()?;
        let probed = probe(src);
        if let Some(m) = &probed
            && let Ok(json) = serde_json::to_vec_pretty(m)
        {
            let _ = write_atomic(&dir.join("meta.json"), &json);
        }
        let seek = probed
            .as_ref()
            .and_then(|m| m.duration)
            .map(|d| (d * recipe.seek_fraction as f64).max(0.0))
            .unwrap_or(0.0);
        let codec = probed.as_ref().and_then(|m| m.codec.clone());
        extract_frame(src, &jpg, seek, recipe, codec.as_deref(), hw)?;
        log::debug!("thumb generated: {}", src.display());
    }
    Some(jpg)
}

/// Generate/serve the animated sprite sheet: ANIM_FRAMES frames sampled
/// evenly across the clip, each fit to the source's true aspect (no
/// crop), tiled into one JPEG.
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
    let dir = entry_dir(root, src, meta.len(), mtime_secs(&meta));
    let jpg = dir.join(recipe.anim_file());

    if !jpg.exists() {
        if !have_ffmpeg {
            return None;
        }
        std::fs::create_dir_all(&dir).ok()?;
        // Sampling frames across the clip needs its duration.
        // The thumb pass usually cached the probe already.
        let m = cached_meta(src).or_else(|| probe(src))?;
        let duration = m.duration.filter(|d| *d > 0.05)?;
        let g = recipe.anim_grid.max(1);
        let frames = (g * g) as usize;
        let (fw, fh) = recipe.anim_frame();
        let q = recipe.quality.clamp(2, 31).to_string();
        // Fit each frame INSIDE the cell box, preserving the source's true
        // aspect — no crop. (The old `increase,crop` center-cropped every
        // cell to 16:9 for the removed grid background animation; the
        // storyboard/chapter consumers then cropped again to display. Both
        // chops are gone.) All g² frames share the source aspect, so they
        // share dims and tile cleanly; `force_divisible_by=2` keeps the
        // mjpeg 4:2:0 encode happy. The sheet's real dims flow out through
        // `decode_jpeg`, so every consumer sizes cells from them.
        let vf =
            format!("scale={fw}:{fh}:force_original_aspect_ratio=decrease:force_divisible_by=2");

        // g² individual seeked extracts, then one cheap tile pass over
        // the tiny jpegs. Each extract is a keyframe-only grab
        // (-noaccurate_seek, below) — one decoded frame, no decode-forward
        // (VT is deliberately off — see the per-extract note below).
        // The old single-command `fps=` filter decoded the ENTIRE clip
        // in software — minutes of multi-core churn per long 4K source,
        // three workers wide, surviving app quit. Never again.
        // Per-cell staging files carry a per-generation uid (P0.6): a
        // concurrent duplicate generation (gen sweep + visible request on
        // separate workers) must not overwrite this run's cells mid-tile.
        let uid = format!("{}-{}", std::process::id(), staging_seq());
        let frame_tmp = |k: usize| dir.join(format!("animf_{uid}_{k}.jpg"));
        let cleanup = |n: usize| {
            for k in 0..n {
                let _ = std::fs::remove_file(frame_tmp(k));
            }
        };
        // Anim-gen latency diagnostic (RUST_LOG=sb_media=debug): per-cell
        // and total extract wall time. With keyframe-only extraction each
        // cell is a single decoded frame, but concurrent gen-sweep 4K
        // decodes still inflate every cell under contention — this log is what let
        // us attribute the "chapter chips take ~a minute" delay to sweep
        // starvation rather than disk or the prewarm gates (see
        // benchmarks/reports/chapter_sheet_latency.md).
        let gen_t0 = std::time::Instant::now();
        // Extract all g² cells in ONE ffmpeg process: g² inputs, each a fast
        // input `-ss` seek, mapped to one mjpeg output. This amortizes the
        // per-process startup + decoder init that DOMINATED the sheet's wall
        // time — nine separate 4K spawns cost ~8s (each paying its own
        // VideoToolbox session init), one process ~2.5s
        // (benchmarks/reports/chapter_sheet_latency.md, lever A). Software
        // decode, no VideoToolbox: for single-frame extracts inside one
        // process sw is actually FASTER than VT (no per-input VT session init)
        // AND leaves the media engine entirely to the watched stream — the
        // storyboard is user-attention work but must not steal the hardware
        // decoder the video the user is watching runs on.
        let seeks: Vec<String> = (0..frames)
            .map(|k| format!("{:.3}", duration * (k as f64 + 0.5) / frames as f64))
            .collect();
        let mut cmd = media_cmd("ffmpeg");
        cmd.args(["-y", "-v", "error"]);
        for s in &seeks {
            // -noaccurate_seek: land on the nearest keyframe and grab IT,
            // skipping the decode-forward to the exact timestamp. On sparse
            // -keyframe 4K (multi-second GOPs) accurate seek decoded a whole
            // GOP per cell in software; the keyframe is plenty precise for a
            // g² storyboard and costs a single decoded frame.
            cmd.args(["-ss", s, "-noaccurate_seek"]).arg("-i").arg(src);
        }
        for k in 0..frames {
            let map = format!("{k}:v:0");
            cmd.args([
                "-map", map.as_str(), "-frames:v", "1", "-vf", vf.as_str(), "-q:v", q.as_str(),
                "-strict", "unofficial", "-f", "mjpeg",
            ])
            .arg(frame_tmp(k));
        }
        let _ = cmd.stdin(Stdio::null()).output();
        // Per-cell fallback: any frame the batch missed (a seek past EOF on a
        // VFR/short source) is retried alone from the clip start.
        for k in 0..frames {
            if frame_tmp(k).exists() {
                continue;
            }
            let ok = media_cmd("ffmpeg")
                .args(["-y", "-v", "error", "-ss", "0", "-noaccurate_seek"])
                .arg("-i")
                .arg(src)
                .args([
                    "-frames:v", "1", "-vf", vf.as_str(), "-q:v", q.as_str(),
                    "-strict", "unofficial", "-f", "mjpeg",
                ])
                .arg(frame_tmp(k))
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success())
                && frame_tmp(k).exists();
            if !ok {
                log::debug!("anim frame {k} failed for {}", src.display());
                cleanup(frames);
                return None;
            }
        }
        log::debug!(
            "animgen {}: {frames} cells in {:.2}s (single process)",
            src.display(),
            gen_t0.elapsed().as_secs_f32(),
        );
        let tile_t0 = std::time::Instant::now();

        let tmp = staging_path(&jpg);
        let pattern = dir.join(format!("animf_{uid}_%d.jpg"));
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
        log::debug!(
            "animgen {}: tile pass {:.2}s (total {:.2}s)",
            src.display(),
            tile_t0.elapsed().as_secs_f32(),
            gen_t0.elapsed().as_secs_f32(),
        );
    }
    decode_jpeg(&jpg, recipe.thumb_w, recipe.thumb_h)
}

/// Cached probe results — a snapshot for humans and future features.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
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
    /// Source pixel format (`yuv420p`, `p010le`, …). Gates the hardware
    /// scale chain: `hwdownload` needs the raw format named explicitly
    /// and 8- vs 10-bit take different ones. Absent in older meta.json —
    /// those clips play through the software chain until a background
    /// reprobe heals the cache entry (`request_reprobe`).
    #[serde(default)]
    pub pix_fmt: Option<String>,
}

/// Read the cached meta.json for a clip without probing (cheap: one small
/// file read; no process spawn). Present for anything with a thumbnail.
///
/// Strictly read-only (P0.7): live spawns call this on the render thread,
/// so a stale `src` (a rename under `size_mtime` keying strands the
/// recorded path, and `--cleanup-cache` judges entries dead by whether
/// meta.src still fingerprints) comes back as-is — the caller queues a
/// background reprobe, whose worker rewrites the entry off-thread. The
/// heal still happens on first access, just never on this thread.
pub fn cached_meta(path: &Path) -> Option<Meta> {
    cached_meta_in(&cache_root(), path)
}

fn cached_meta_in(root: &Path, path: &Path) -> Option<Meta> {
    let meta = std::fs::metadata(path).ok()?;
    let file = entry_dir(root, path, meta.len(), mtime_secs(&meta)).join("meta.json");
    serde_json::from_slice(&std::fs::read(&file).ok()?).ok()
}

/// Worker-side half of `request_reprobe`: probe again and rewrite the
/// clip's meta.json in place. Skips entries that already carry every
/// gating field (duplicate queue entries and cross-worker races both
/// land here as cheap no-ops — last write wins and the writes agree).
fn reprobe(src: &Path, root: &Path) {
    let Ok(st) = std::fs::metadata(src) else {
        return;
    };
    let dir = entry_dir(root, src, st.len(), mtime_secs(&st));
    let file = dir.join("meta.json");
    if let Ok(bytes) = std::fs::read(&file)
        && let Ok(mut m) = serde_json::from_slice::<Meta>(&bytes)
        && m.pix_fmt.is_some()
    {
        // Entry is complete — no probe needed. But a stale `src` (rename
        // under size_mtime keying) still wants rewriting: cached_meta no
        // longer heals it inline (P0.7 — that write ran on the render
        // thread), so the worker owns it now.
        if m.src != src {
            m.src = src.to_path_buf();
            if let Ok(json) = serde_json::to_vec_pretty(&m) {
                let _ = write_atomic(&file, &json);
                log::debug!("meta src healed: {}", src.display());
            }
        }
        return;
    }
    let Some(m) = probe(src) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_vec_pretty(&m) {
        let _ = write_atomic(&file, &json);
        log::debug!("meta healed: {}", src.display());
    }
}

/// Chapter starts + container duration in one ffprobe run. The chapter
/// list is deliberately NOT in the cached `Meta` (old entries would
/// wrongly report "no chapters" forever); the bar is transient enough
/// that one fresh probe per open is fine.
fn probe_chapter_info(src: &Path) -> Option<(Vec<f64>, Option<f64>)> {
    let out = media_cmd("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_chapters",
            "-show_format",
        ])
        .arg(src)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let mut chapters: Vec<f64> = v["chapters"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .filter_map(|c| c["start_time"].as_str().and_then(|s| s.parse().ok()))
                .collect()
        })
        .unwrap_or_default();
    chapters.sort_by(|a: &f64, b: &f64| a.total_cmp(b));
    let duration = v["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse().ok());
    Some((chapters, duration))
}

/// Probe a clip with ffprobe (niced, blocking — do not call from the
/// render path; the cache normally supplies this via `cached_meta`).
/// Public for the pacebench example and future tooling.
pub fn probe(src: &Path) -> Option<Meta> {
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
        pix_fmt: video.and_then(|s| s["pix_fmt"].as_str().map(String::from)),
    })
}

/// A staging path beside `dst`, unique per generation (P0.6): the name
/// carries pid + a process-wide sequence number, so two workers — or two
/// switchblade processes — generating the same missing artifact can never
/// write the same temp file (a deterministic name let their ffmpeg
/// outputs interleave and renamed a truncated JPEG into the cache). The
/// duplicate WORK still happens until artifacts are coalesced (P1.2);
/// with unique staging plus atomic rename, the last complete file simply
/// wins. Stale staging files from a crash are swept by `--cleanup-cache`.
fn staging_path(dst: &Path) -> PathBuf {
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{}.tmp", std::process::id(), staging_seq()));
    dst.with_file_name(name)
}

/// Process-wide staging sequence (also keys `make_anim`'s per-cell files).
fn staging_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Publish a small file atomically (staging + rename) — concurrent
/// writers of the same `meta.json` must not interleave (P0.6).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = staging_path(path);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn extract_frame(
    src: &Path,
    dst: &Path,
    seek: f64,
    recipe: &Recipe,
    codec: Option<&str>,
    // Allow VideoToolbox. FALSE for the sustained background sweep (gen)
    // and anim sheets: a single seeked frame decodes fine in software
    // (niced, cheap), and keeping niced sweep work off the VT media
    // engine leaves that shared hardware unit for the live stream the
    // user is actually watching — `nice` scopes CPU, not the media
    // engine, so a full-throttle VT sweep still hitched live playback.
    hw: bool,
) -> Option<()> {
    // Write to a unique temp name and rename, so a half-written jpg never
    // looks like a cache hit to another worker or a later run — and a
    // concurrent duplicate generation can't interleave into the same file.
    let tmp = staging_path(dst);
    let (tw, th) = (recipe.thumb_w, recipe.thumb_h);
    let q = recipe.quality.clamp(2, 31).to_string();
    let vf = format!("scale={tw}:{th}:force_original_aspect_ratio=decrease");
    // stderr is captured, not inherited: decode noise from damaged files
    // must never spam the console (it becomes one debug line below).
    // `-strict unofficial` lets mjpeg accept full-range YUV sources
    // (common in phone and AI-generated footage; hard error in ffmpeg 8+).
    // Foreground (hw): niced CPU, normal I/O, VideoToolbox. Background
    // sweep (!hw): software decode AND background-band throttled disk
    // I/O — see `media_cmd_bg`.
    let mut cmd = if hw {
        media_cmd("ffmpeg")
    } else {
        media_cmd_bg("ffmpeg")
    };
    cmd.args(["-y", "-v", "error"]);
    if hw && vt_accel(codec) {
        cmd.args(["-hwaccel", "videotoolbox"]);
    }
    let out = cmd
        // -noaccurate_seek: grab the nearest keyframe ≤ seek, no
        // decode-forward to the exact timestamp. The live stream's initial
        // seek is keyframe too (seekable.rs / LivePlayer::spawn), so thumb
        // and first live frame still land on the SAME frame — the no-jolt
        // handoff holds, now without the per-thumb GOP decode that slowed
        // the whole gen sweep on sparse-keyframe 4K.
        .args(["-ss", &format!("{seek:.3}"), "-noaccurate_seek"])
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
            return extract_frame(src, dst, 0.0, recipe, codec, hw);
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

/// How cache entries are keyed to source files (config `cache_key`,
/// PLAN.md §8). Startup-only: flipping it under a running cache would
/// split entries across two keyings mid-session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheKey {
    /// Absolute path + size + mtime — the original MVP key. Renamed or
    /// moved files lose their cache (it regenerates elsewhere).
    Path,
    /// Size + mtime only (default): entries survive renames and moves
    /// (rating renames, library reshuffles). The collision needing two
    /// different files with the same byte size AND the same mtime second
    /// is vanishingly rare in a clip library, and its cost is a wrong
    /// thumb until `--cleanup-cache`; exact duplicates deliberately share
    /// one entry.
    #[default]
    SizeMtime,
}

static CACHE_KEY: std::sync::OnceLock<CacheKey> = std::sync::OnceLock::new();

/// Install the configured key. Call once at startup before any cache
/// access; later calls are ignored (first write wins, like `cache_root`).
pub fn set_cache_key(key: CacheKey) {
    let _ = CACHE_KEY.set(key);
}

fn active_cache_key() -> CacheKey {
    *CACHE_KEY.get_or_init(CacheKey::default)
}

/// Pragmatic MVP fingerprint (PLAN.md §8): what goes into it depends on
/// the configured `CacheKey`. FNV-1a so the key is stable across runs
/// and toolchains. Stronger modes (partial content hash) come later.
/// Always reached through `entry_dir` (which layers on lazy adoption);
/// `maintenance` calls it directly to test an entry against both keys.
fn fingerprint_with(key: CacheKey, path: &Path, size: u64, mtime: u64) -> String {
    let path_bytes: &[u8] = match key {
        CacheKey::Path => path.as_os_str().as_encoded_bytes(),
        CacheKey::SizeMtime => &[],
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for chunk in [path_bytes, &size.to_le_bytes(), &mtime.to_le_bytes()] {
        for &b in chunk {
            h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

/// Resolve a source file's cache entry directory under the active key.
/// Under `size_mtime`, an entry the old `path` keying left behind is
/// adopted (renamed across) the first time the file is seen — switching
/// keys must not regenerate a library-sized cache.
fn entry_dir(root: &Path, src: &Path, size: u64, mtime: u64) -> PathBuf {
    entry_dir_with(active_cache_key(), root, src, size, mtime)
}

fn entry_dir_with(key: CacheKey, root: &Path, src: &Path, size: u64, mtime: u64) -> PathBuf {
    let fp = fingerprint_with(key, src, size, mtime);
    let dir = root.join(&fp[..2]).join(&fp);
    if key != CacheKey::Path && !dir.exists() {
        let old_fp = fingerprint_with(CacheKey::Path, src, size, mtime);
        let old = root.join(&old_fp[..2]).join(&old_fp);
        if old.is_dir() {
            let ok = std::fs::create_dir_all(dir.parent().unwrap()).is_ok()
                && std::fs::rename(&old, &dir).is_ok();
            if ok {
                log::debug!("cache entry adopted under size_mtime: {}", src.display());
            }
        }
    }
    dir
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

    /// `size_mtime` keying ignores the path (renames/moves keep the
    /// cache); `path` keying doesn't. Size or mtime changes always
    /// re-key — the file's bytes changed, its artifacts are stale.
    #[test]
    fn cache_key_modes_fingerprint_as_documented() {
        let (a, b) = (Path::new("/x/a.mp4"), Path::new("/y/b ★★★.mp4"));
        let path_key = |p, s, m| fingerprint_with(CacheKey::Path, p, s, m);
        let stat_key = |p, s, m| fingerprint_with(CacheKey::SizeMtime, p, s, m);
        assert_ne!(path_key(a, 10, 20), path_key(b, 10, 20));
        assert_eq!(stat_key(a, 10, 20), stat_key(b, 10, 20));
        assert_ne!(stat_key(a, 10, 20), stat_key(a, 11, 20));
        assert_ne!(stat_key(a, 10, 20), stat_key(a, 10, 21));
        // The two keyings never collide for the same file, so adoption
        // (below) can tell them apart.
        assert_ne!(path_key(a, 10, 20), stat_key(a, 10, 20));
    }

    /// Switching to `size_mtime` adopts an existing path-keyed entry by
    /// renaming it across — a library-sized cache must not regenerate
    /// because the keying changed.
    #[test]
    fn size_mtime_adopts_path_keyed_entries() {
        let root = std::env::temp_dir().join("sb_media_adopt_test");
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("clip.mp4");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&src, b"not really a video").unwrap();
        let st = std::fs::metadata(&src).unwrap();
        let (len, mt) = (st.len(), mtime_secs(&st));

        // The path-keyed era left an entry with artifacts.
        let old_fp = fingerprint_with(CacheKey::Path, &src, len, mt);
        let old = root.join(&old_fp[..2]).join(&old_fp);
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("thumb_fit_640x360_q5.jpg"), b"jpg").unwrap();

        let new = entry_dir_with(CacheKey::SizeMtime, &root, &src, len, mt);
        assert_ne!(new, old);
        assert!(!old.exists(), "old entry should have been renamed across");
        assert!(new.join("thumb_fit_640x360_q5.jpg").exists());
        // Second resolve is a plain hit, no rename left to do.
        assert_eq!(
            entry_dir_with(CacheKey::SizeMtime, &root, &src, len, mt),
            new
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `cached_meta` runs on the render thread, so it must never write
    /// (P0.7 — it used to heal a stale `src` inline); the worker-side
    /// reprobe owns the heal now, and does it WITHOUT an ffprobe when
    /// the entry is otherwise complete.
    #[test]
    fn cached_meta_is_read_only_and_reprobe_heals_src() {
        let root = std::env::temp_dir().join("sb_media_meta_heal_test");
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("clip.mp4");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&src, b"not really a video").unwrap();
        let st = std::fs::metadata(&src).unwrap();
        let dir = entry_dir(&root, &src, st.len(), mtime_secs(&st));
        std::fs::create_dir_all(&dir).unwrap();

        // A complete entry whose recorded src is stranded by a rename.
        let stale = Meta {
            src: PathBuf::from("/somewhere/else/clip.mp4"),
            duration: Some(1.0),
            width: Some(64),
            height: Some(36),
            codec: Some("h264".into()),
            fps: Some(30.0),
            rotation: None,
            pix_fmt: Some("yuv420p".into()),
        };
        let file = dir.join("meta.json");
        std::fs::write(&file, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();
        let before = std::fs::read(&file).unwrap();

        // The read returns the disk truth untouched — stale src and all
        // (the caller detects it and queues the heal).
        let m = cached_meta_in(&root, &src).expect("meta reads");
        assert_eq!(m.src, stale.src, "read-only: src comes back as stored");
        assert_eq!(
            std::fs::read(&file).unwrap(),
            before,
            "cached_meta must not write"
        );

        // The reprobe worker heals src in place. The source is garbage,
        // so this passing also proves no ffprobe ran (the entry already
        // carries pix_fmt — a probe would have failed and written nothing).
        reprobe(&src, &root);
        let healed: Meta =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).expect("healed meta parses");
        assert_eq!(healed.src, src, "worker rewrote the stranded src");
        assert_eq!(healed.pix_fmt.as_deref(), Some("yuv420p"));
        assert_eq!(healed.duration, Some(1.0), "probe fields survive the heal");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The quickview storyboard's sheet must outrank the library-wide
    /// gen sweep: after a thumb-recipe change the sweep holds thousands
    /// of entries for hours, and the strict tiers starved the on-demand
    /// sheet below it — hover thumbs silently never arrived. AND while a
    /// sheet is in flight the gen sweep is paused entirely (user-attention
    /// work must not fight a concurrent 4K sweep decode), so the sweep only
    /// resumes once the sheet completes.
    #[test]
    fn quickview_sheet_outranks_gen_sweep_but_not_visible_thumbs() {
        let mut q = Queues::default();
        q.r#gen.push_back("sweep".into());
        q.anims_now.push_back("storyboard".into());
        q.chapters.push_back("chapter-probe".into());
        q.reprobes.push_back("heal".into());
        q.thumbs.push_back("visible".into());
        // Drain, completing each job as a real worker does — crucially the
        // anim, since the sweep is paused until the in-flight sheet finishes.
        let mut order = Vec::new();
        while let Some(r) = q.pop() {
            let (kind, p) = match &r {
                Request::Thumb(p) => ("thumb", p.clone()),
                Request::Reprobe(p) => ("reprobe", p.clone()),
                Request::Chapters(p) => ("chapters", p.clone()),
                Request::Gen(p) => ("gen", p.clone()),
                Request::Anim(p) => ("anim", p.clone()),
            };
            order.push(format!("{kind}:{}", p.display()));
            match &r {
                Request::Anim(p) => {
                    q.complete(p, Art::Anim);
                }
                Request::Gen(p) => {
                    q.gen_running -= 1;
                    q.complete(p, Art::Thumb);
                }
                _ => {}
            }
        }
        assert_eq!(
            order,
            [
                "thumb:visible",
                "reprobe:heal",
                "chapters:chapter-probe",
                "anim:storyboard",
                "gen:sweep",
            ]
        );

        // The pause is real: with a sheet queued (not yet completed), the
        // sweep does NOT run even though a gen job is waiting.
        let mut q = Queues::default();
        q.r#gen.push_back("sweep".into());
        q.anims_now.push_back("storyboard".into());
        assert!(
            matches!(q.pop(), Some(Request::Anim(_))),
            "the sheet is taken first"
        );
        assert!(
            q.pop().is_none(),
            "gen stays paused while the sheet is in flight"
        );
    }

    /// Meta for the generated test clips (h264 yuv420p @30) — on macOS
    /// this takes the hardware scale chain, so the pacing tests cover it.
    fn test_meta(clip: &Path) -> Meta {
        Meta {
            src: clip.to_path_buf(),
            duration: None,
            width: None,
            height: None,
            codec: Some("h264".into()),
            fps: Some(30.0),
            rotation: None,
            pix_fmt: Some("yuv420p".into()),
        }
    }

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

        let player =
            LivePlayer::spawn(&clip, 320, 180, 0.4, Some(&test_meta(&clip))).expect("spawn");
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

    /// The chapter probe behind the fullview chapter bar: a chaptered
    /// clip reports its real chapter starts (ascending), a chapterless
    /// one reports an empty list — both with the container duration the
    /// app's synthesized-checkpoint fallback needs. Needs ffprobe.
    #[test]
    fn chapter_probe_reports_starts_and_duration() {
        if !have_binary("ffmpeg") || !have_binary("ffprobe") {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_media_chapter_test");
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("plain.mp4");
        if !plain.exists() {
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
                .arg(&plain)
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok, "failed to generate test clip");
        }
        // Same clip with three chapters muxed in via ffmetadata.
        let chaptered = dir.join("chaptered.mp4");
        if !chaptered.exists() {
            let metafile = dir.join("chapters.txt");
            std::fs::write(
                &metafile,
                ";FFMETADATA1\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1000\ntitle=one\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=1000\nEND=2500\ntitle=two\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=2500\nEND=4000\ntitle=three\n",
            )
            .unwrap();
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error"])
                .arg("-i")
                .arg(&plain)
                .arg("-i")
                .arg(&metafile)
                .args(["-map_metadata", "1", "-codec", "copy"])
                .arg(&chaptered)
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok, "failed to mux chapters");
        }

        // No chapters: empty list, real duration.
        let (times, duration) = probe_chapter_info(&plain).expect("probe plain");
        assert!(times.is_empty(), "chapterless clip reports no chapters");
        assert!(
            duration.is_some_and(|d| (3.5..=4.5).contains(&d)),
            "container duration reported, got {duration:?}"
        );

        // Chapters: the real starts, ascending.
        let (times, duration) = probe_chapter_info(&chaptered).expect("probe chaptered");
        assert_eq!(times.len(), 3, "one entry per chapter");
        assert!((times[0] - 0.0).abs() < 0.05, "first chapter start");
        assert!((times[1] - 1.0).abs() < 0.05, "second chapter start");
        assert!((times[2] - 2.5).abs() < 0.05, "third chapter start");
        assert!(duration.is_some(), "duration comes along with chapters");
    }

    /// A visible-thumb request absorbs the gen sweep's queued entry for
    /// the same artifact (P1.2): one generation runs, at tier 1, and the
    /// sweep's `GenDone` is owed to the thumb job.
    #[test]
    fn visible_request_absorbs_queued_gen() {
        let mut q = Queues::default();
        let p = PathBuf::from("clip.mp4");
        q.push_gen(p.clone());
        q.push_thumb(p.clone());
        let Some(Request::Thumb(popped)) = q.pop() else {
            panic!("expected the (promoted) thumb job first");
        };
        assert_eq!(popped, p);
        assert!(q.pop().is_none(), "the absorbed gen must not run twice");
        let owed = q.complete(&p, Art::Thumb);
        assert_eq!((owed.gens, owed.thumbs), (1, 0), "sweep's GenDone owed");
    }

    /// A visible request arriving while the gen sweep is ALREADY
    /// generating that artifact joins it (owed foreground decode) instead
    /// of launching a duplicate ffprobe/ffmpeg run (P1.2).
    #[test]
    fn visible_request_joins_inflight_gen() {
        let mut q = Queues::default();
        let p = PathBuf::from("clip.mp4");
        q.push_gen(p.clone());
        assert!(matches!(q.pop(), Some(Request::Gen(_)))); // worker took it
        q.push_thumb(p.clone());
        assert!(q.pop().is_none(), "no duplicate generation queued");
        let owed = q.complete(&p, Art::Thumb);
        assert_eq!(
            (owed.gens, owed.thumbs),
            (0, 1),
            "one foreground decode owed; the job's own GenDone covers the sweep"
        );
    }

    /// While the selected stream presents, the gen sweep narrows to
    /// `gen_live_cap` concurrent jobs — the full pool's seeked 4K extracts
    /// contend for CPU + the VT media engine during a stream's cold spawn
    /// (benchmarked ~45% slower time-to-first-frame uncapped). Higher tiers
    /// are never capped, a finishing job frees its slot, and clearing `live`
    /// reopens the sweep to every worker.
    #[test]
    fn live_playback_caps_gen_sweep_concurrency() {
        let mut q = Queues::default();
        q.gen_live_cap = 1; // config default; Queues::default leaves it 0 (unlimited)
        for i in 0..3 {
            q.push_gen(PathBuf::from(format!("clip{i}.mp4")));
        }
        q.live = true;
        let Some(Request::Gen(first)) = q.pop() else {
            panic!("one gen job runs under the live cap");
        };
        assert!(q.pop().is_none(), "the cap parks the other workers");
        // Visible work is never capped — only the sweep is.
        q.push_thumb(PathBuf::from("visible.mp4"));
        assert!(matches!(q.pop(), Some(Request::Thumb(_))));
        // The finishing job frees its slot for exactly one more.
        q.gen_running -= 1;
        q.complete(&first, Art::Thumb);
        assert!(matches!(q.pop(), Some(Request::Gen(_))));
        assert!(q.pop().is_none());
        // Nothing playing: the whole pool sweeps again.
        q.live = false;
        assert!(matches!(q.pop(), Some(Request::Gen(_))));
    }

    /// Duplicate gen requests (D-swap re-ingests the same files) coalesce
    /// to one queued job that owes the extra `GenDone`s — progress
    /// accounting stays balanced (P1.2).
    #[test]
    fn duplicate_gen_requests_coalesce() {
        let mut q = Queues::default();
        let p = PathBuf::from("clip.mp4");
        q.push_gen(p.clone());
        q.push_gen(p.clone());
        assert!(matches!(q.pop(), Some(Request::Gen(_))));
        assert!(q.pop().is_none());
        assert_eq!(q.complete(&p, Art::Thumb).gens, 1);
    }

    /// Duplicate on-demand sheet requests (a D swap re-requests while
    /// the first job is in flight) coalesce to one generation that owes
    /// the extra results (P1.2).
    #[test]
    fn duplicate_sheet_requests_coalesce() {
        let mut q = Queues::default();
        let p = PathBuf::from("sheet.mp4");
        q.push_anim_now(p.clone());
        q.push_anim_now(p.clone()); // queued duplicate merges
        assert!(matches!(q.pop(), Some(Request::Anim(_)))); // worker took it
        q.push_anim_now(p.clone()); // in-flight duplicate merges too
        assert!(q.pop().is_none(), "one generation, no duplicates");
        assert_eq!(q.complete(&p, Art::Anim).anims, 2, "merged results owed");
    }

    /// The thumb seek fraction is part of the artifact name so a changed
    /// start point regenerates — but the historical default keeps the
    /// original suffix-less name so existing caches survive an upgrade.
    #[test]
    fn seek_fraction_names_the_thumb_artifact() {
        let base = Recipe {
            thumb_w: 640,
            thumb_h: 360,
            quality: 5,
            anim_grid: 3,
            seek_fraction: 0.10,
        };
        assert_eq!(
            base.thumb_file(),
            "thumb_fit_640x360_q5.jpg",
            "the default fraction keeps the legacy name"
        );
        let start = Recipe {
            seek_fraction: 0.0,
            ..base
        };
        assert_eq!(
            start.thumb_file(),
            "thumb_fit_640x360_q5_s0.jpg",
            "a 0% start gets its own artifact"
        );
        let quarter = Recipe {
            seek_fraction: 0.25,
            ..base
        };
        assert_eq!(quarter.thumb_file(), "thumb_fit_640x360_q5_s25.jpg");
    }

    /// Two generations of the same artifact must never share a staging
    /// file — a shared name let concurrent ffmpeg outputs interleave and
    /// renamed a truncated JPEG into the cache (P0.6).
    #[test]
    fn staging_paths_are_unique_per_generation() {
        let dst = Path::new("/cache/ab/entry/thumb_fit_640x360_q5.jpg");
        let a = staging_path(dst);
        let b = staging_path(dst);
        assert_ne!(a, b, "two generations shared a staging path");
        assert_eq!(
            a.parent(),
            dst.parent(),
            "staging stays beside the artifact"
        );
        assert!(a.to_string_lossy().ends_with(".tmp"));
    }

    /// Concurrent duplicate generation of one missing artifact (gen sweep
    /// and visible request on separate workers — coalescing is P1.2's
    /// job) must still publish a decodable file: unique staging and
    /// atomic rename mean whichever complete file lands last wins (P0.6).
    #[test]
    fn concurrent_generation_publishes_a_clean_artifact() {
        if !have_binary("ffmpeg") || !have_binary("ffprobe") {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_media_race_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("race.mp4");
        if !clip.exists() {
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=2:size=320x180:rate=30")
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
        let recipe = Recipe {
            thumb_w: 320,
            thumb_h: 180,
            quality: 5,
            anim_grid: 2,
            seek_fraction: 0.10,
        };
        let root = dir.join("race-cache");
        for _ in 0..3 {
            let _ = std::fs::remove_dir_all(&root);
            let t = {
                let (clip, root) = (clip.clone(), root.clone());
                thread::spawn(move || ensure_thumb_file(&clip, &root, true, &recipe, true))
            };
            let here = ensure_thumb_file(&clip, &root, true, &recipe, true);
            let there = t.join().unwrap();
            let jpg = here.or(there).expect("at least one generation succeeds");
            assert!(
                decode_jpeg(&jpg, 320, 180).is_some(),
                "published artifact must decode cleanly"
            );
            let meta: Result<Meta, _> =
                serde_json::from_slice(&std::fs::read(jpg.with_file_name("meta.json")).unwrap());
            assert!(meta.is_ok(), "meta.json must survive concurrent writers");
        }
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
            seek_fraction: 0.10,
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

    /// A non-16:9 source must produce true-aspect cells — fit into the
    /// cell box, NOT center-cropped to 16:9 (the removed grid-animation
    /// crop that this change undoes). A square source → square cells.
    #[test]
    fn anim_sheet_cells_keep_source_aspect() {
        if !have_binary("ffmpeg") || !have_binary("ffprobe") {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_media_anim_aspect_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("anim_square.mp4");
        if !clip.exists() {
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=4:size=240x240:rate=30")
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
        let _ = std::fs::remove_dir_all(&root);
        let recipe = Recipe {
            thumb_w: 320,
            thumb_h: 180,
            quality: 5,
            anim_grid: 3,
            seek_fraction: 0.10,
        };
        let (w, h, _) = make_anim(&clip, &root, true, &recipe).expect("sheet generated");
        // Cell = sheet / grid; a square source must yield ~square cells,
        // never the 16:9 the crop-fill would have forced.
        let (cw, ch) = (w as f32 / 3.0, h as f32 / 3.0);
        let cell_a = cw / ch;
        assert!(
            (cell_a - 1.0).abs() < 0.1,
            "square source should give square cells, got {cw}x{ch} (a={cell_a})"
        );
    }

    /// Dropping a player must release its reader thread even when that
    /// reader is parked on the full-queue condvar (every warm player's
    /// steady state). The leak: kill() only unblocks read_exact, so
    /// parked readers pinned ~30MB each, forever, per selection change —
    /// live video degraded after browsing a while.
    #[test]
    fn dropped_player_releases_its_reader() {
        if !have_binary("ffmpeg") {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_media_pace_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("pace_drop.mp4");
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

        let player =
            LivePlayer::spawn(&clip, 320, 180, 0.4, Some(&test_meta(&clip))).expect("spawn");
        // Never drain: the queue fills and the reader parks on `space`.
        // Wait until it actually has (bounded, not a fixed sleep — the
        // fixed 700ms was contention-flaky in parallel test runs; T1).
        assert!(
            wait_buffered(
                &player,
                LIVE_QUEUE_DEPTH,
                std::time::Duration::from_secs(15)
            ),
            "queue never filled"
        );
        // The reader holds the only other strong ref to the queue; if the
        // weak ref dies after drop, the thread exited.
        let queue = Arc::downgrade(&player.queue);
        drop(player);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while queue.upgrade().is_some() && std::time::Instant::now() < deadline {
            thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(queue.upgrade().is_none(), "reader thread leaked after drop");
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

        let player =
            LivePlayer::spawn(&clip, 320, 180, 0.4, Some(&test_meta(&clip))).expect("spawn");
        // Warm without watching: the queue caps at LIVE_QUEUE_DEPTH and
        // the decoder stalls behind it. Wait until buffered (bounded,
        // not a fixed sleep — contention-flaky in parallel runs; T1).
        assert!(
            wait_buffered(
                &player,
                LIVE_QUEUE_DEPTH,
                std::time::Duration::from_secs(15)
            ),
            "queue never filled while unwatched"
        );
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

    /// Bounded wait until the player has buffered at least `n` frames.
    /// The fixed sleeps this replaces passed serially but flaked under
    /// parallel-suite CPU contention (T1).
    fn wait_buffered(p: &LivePlayer, n: usize, within: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if p.buffered() >= n {
                return true;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }
}
