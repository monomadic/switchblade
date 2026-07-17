//! sb-app: application state and grid logic, headless of any OS/GPU types.
//! Implements the `sb_window::App` trait (PLAN.md §12).

mod commands;
mod ingest;
mod tuning;

pub use tuning::{AnimLevel, Tuning, config_path};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use sb_media::{MediaService, Recipe, ThumbResult};
use sb_window::{
    App, AtlasCfg, Blur, Frame, HiresFrame, InputEvent, Key, ThumbUpload, Tile, Viewport, Waker,
    WindowCommand,
};

use commands::{Action, KeyMap};
use tuning::{TuningFile, alpha};

/// Rows beyond the viewport to prefetch thumbnails for.
const PREFETCH_ROWS: usize = 2;
/// After a live decoder reports failure, don't respawn one for that path
/// for this long — a permanently broken clip must not enter a rapid
/// spawn-fail loop, while a transient failure (file busy, network mount
/// blip) still recovers on a later attempt (P0.4).
const LIVE_RETRY_COOLDOWN_S: f32 = 30.0;
/// Ingest items accepted per frame (P0.3): a fast directory walk must
/// not stall one frame appending thousands of clips — the backlog
/// catches up over the next frames (the waker schedules them).
const INGEST_DRAIN_BUDGET: usize = 256;
/// Texture uploads accepted from media results per frame (P0.3): a
/// warm-cache burst must not issue hundreds of texture writes in one
/// frame. Results without pixels (Failed/GenDone) don't count.
const MEDIA_UPLOAD_BUDGET: usize = 64;
/// The much tighter upload budget while the selected stream is actively
/// playing: 64 thumbs at the default recipe stage ~80MB of texture
/// writes between two video presents — a visible hitch. The backlog
/// trickles in over a few frames instead (still ~700/s at 60fps); the
/// full budget returns the moment nothing is playing.
const MEDIA_UPLOAD_BUDGET_LIVE: usize = 12;
/// Background prewarms (fullview's chapter probe + anim sheet) normally
/// wait for the selected stream's first frame; a stream that never
/// delivers (failed decode, retry cooldown) releases them after this
/// grace instead of starving the storyboard forever.
const PREWARM_GRACE_S: f32 = 3.0;
/// Thumbnail crossfade duration once pixels arrive.
const THUMB_FADE_S: f32 = 0.3;
/// Skip flash bar: hold after a `[`/`]` skip, then a quick fade-out.
const SKIP_BAR_HOLD_S: f32 = 1.0;
const SKIP_BAR_FADE_S: f32 = 0.25;
/// Skip-timer stall guard: when a live stream is expected but hasn't
/// delivered a first frame (cold spawn, failed decode, cloud placeholder),
/// the timer still advances after `skip_timer_s` plus this grace — a
/// slideshow must never freeze on one bad clip.
const SKIP_TIMER_SPAWN_GRACE_S: f32 = 3.0;

const DEMO_TILES: usize = 480;

/// Derived per-frame grid metrics; see [`Switchblade::layout`].
struct Layout {
    cols: usize,
    tile_w: f32,
    tile_h: f32,
    cell_w: f32,
    cell_h: f32,
}

enum Thumb {
    /// Not yet requested (or evicted from the atlas; re-requests when visible).
    None,
    Pending,
    /// `tw × th` = pixels within the slot (static thumb: original aspect;
    /// anim: sprite-sheet dimensions).
    Ready {
        slot: usize,
        at: Instant,
        tw: u32,
        th: u32,
    },
    Failed,
}

/// What a given atlas slot holds, so eviction can reset the right state.
#[derive(Clone, Copy)]
enum SlotKind {
    Static,
    Anim,
    /// The live-playback frame for the selected clip; never evicted.
    Live,
}

/// A chapter probe's answer: (real chapter starts, container duration).
type ChapterPlan = (Vec<f64>, Option<f64>);

/// The fullview chapter bar (`chapter_mode`, `g`): filmstrip-style chips
/// that slide up from the bottom of fullview — one per chapter when the
/// file has real chapters, synthesized checkpoints otherwise (none under
/// a minute, 4 over a minute, 8 over three, 10 over ten). Chip images
/// come from the clip's cached anim sheet (the same frames the seekbar
/// storyboard shows) — nothing is extracted for the bar. Clicking a chip
/// seeks the playing stream to that chapter and slides the bar back down.
#[derive(Clone)]
struct ChapterBar {
    /// The clip the bar describes; dropped if the selection leaves it.
    path: PathBuf,
    /// Clip duration (cached meta at open, refined by the probe) — maps
    /// chapter times onto anim-sheet cells and finds the playing chapter.
    duration: Option<f64>,
    /// Chapter start times, ascending: None until the probe answers,
    /// then real chapters or synthesized checkpoints.
    times: Option<Vec<f64>>,
    /// True while the bar wants to be up; false slides it out, and the
    /// state drops once the slide lands.
    open: bool,
    /// 0..1 slide-in from below the bottom edge (also the bar's alpha).
    slide: f32,
    /// Strip scroll position/target in chip units, like the filmstrip —
    /// but panning only: chapters seek on click, never on scroll.
    pos: f32,
    target: f32,
}

/// The hovered tile's video playback: tile-sized, into an atlas slot.
/// Rides `SeekablePlayer` like every live lane since the libav port (it
/// never seeks, but the in-process spawn reaches first frame ~2× sooner
/// than the CLI pipe — hover-play is all about that latency); the CLI
/// `LivePlayer` stays in sb-media as the tested fallback until the
/// in-process path has soaked a release.
struct LiveState {
    clip: usize,
    player: sb_media::SeekablePlayer,
    slot: usize,
    /// Set when the first frame arrives; the tile switches to video then.
    first_frame: Option<Instant>,
}

/// The selected clip's live stream: decoded once at quickview resolution
/// into the mipmapped hires texture. The tile shows it downscaled and the
/// quickview modal shows it big — one decoder, one timeline, no handoffs.
/// Rides the resident in-process decoder (PLAN.md §15 "Low-latency seek"),
/// so `[`/`]` and future scrubbing are real `seek()`s, never respawns.
struct SelLive {
    clip: usize,
    /// The clip's path — survives index churn (the D siblings swap
    /// renumbers every clip while this stream keeps playing).
    path: PathBuf,
    player: sb_media::SeekablePlayer,
    spawned: Instant,
    first_frame: Option<Instant>,
    /// Cached probe duration, for skip targets and the skip flash bar.
    duration: Option<f64>,
}

impl SelLive {
    /// Playback position in seconds: the decoder's real pts (the seek
    /// destination while one is in flight). Callers mod by duration —
    /// looping resets pts to zero on its own.
    fn position(&self) -> f64 {
        self.player.position()
    }
}

struct Clip {
    path: PathBuf,
    readable: bool,
    /// iCloud placeholder — shown with a cloud badge, never read (reading
    /// would trigger a download).
    cloud: bool,
    /// The clip's thumbnail is known to be in the disk cache — set when
    /// its gen-sweep job completes or any artifact delivers. jump_random
    /// and shuffle_library restrict themselves to cached clips so a jump
    /// into unswept territory can't detonate a screenful of on-demand
    /// ffmpeg work.
    cached: bool,
    spawned: Instant,
    scale: f32,
    /// 0..1 emphasis spring: morphs the tile between its grid shape and
    /// the emphasized (true-aspect, cover-fit) shape. Keyboard selection
    /// and hover both ride this same animation.
    emph: f32,
    thumb: Thumb,
    /// Sprite-sheet animation (M6): frames cycle in the grid; the static
    /// thumb stays authoritative for the emphasized tile.
    anim: Thumb,
}

/// Startup options from the CLI (config handles everything else).
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `--animation <level>`: overrides the config's `animation`.
    pub animation: Option<AnimLevel>,
    /// Paths from the CLI; when non-empty they are the input source and
    /// stdin is ignored.
    pub inputs: Vec<PathBuf>,
    /// Force the fake-tile demo grid.
    pub demo: bool,
    /// `--fullscreen` / `--fast-fullscreen`: start fullscreen. The bool
    /// is the fast flag (borderless desktop-sized window instead of
    /// macOS native fullscreen).
    pub fullscreen: Option<bool>,
    /// `--no-config`: run on the internal defaults — no config search,
    /// no hot-reload. For tests and triage, where behavior must not be
    /// steerable by a stray ./switchblade.toml or ~/.config file.
    pub no_config: bool,
}

pub struct Switchblade {
    clips: Vec<Clip>,
    /// Path → clip index, for routing async thumbnail results.
    index: HashMap<PathBuf, usize>,
    rx: Option<Receiver<ingest::Ingested>>,
    media: MediaService,
    /// Atlas slot → owner. Fixed pool shared by static thumbs, anim sheets
    /// and the live frame; class+distance-based eviction (see alloc_slot).
    slots: Vec<Option<(usize, SlotKind)>>,
    /// Live playback: the selected clip's hires stream + the hovered
    /// tile's atlas-slot lane, each started once its target settles.
    live_sel: Option<SelLive>,
    live_hover: Option<LiveState>,
    /// Pre-warmed decoders for the filmstrip neighbors (quickview only),
    /// spawned ahead of need so h/l shows video the same tick. An
    /// unwatched player's bounded frame queue fills and stalls its
    /// decoder after a few frames, so warmth is all but free.
    warm: Vec<SelLive>,
    /// The newest hires frame this tick, routed to Frame.hires_upload.
    hires_frame: Option<HiresFrame>,
    /// Which clip's pixels the hires texture currently holds. Lets a
    /// mid-seek stream keep showing its last frame (the texture still
    /// has it) instead of flashing back to the thumbnail while the new
    /// position decodes in.
    hires_shown: Option<PathBuf>,
    /// The D (siblings) swap in flight: the selected clip's path, kept
    /// playing while the parent-dir listing streams in; when it arrives
    /// it becomes the selection again.
    pending_reselect: Option<PathBuf>,
    /// Paths whose live decoder failed, and when — spawn attempts wait
    /// out `LIVE_RETRY_COOLDOWN_S` (P0.4).
    live_retry: HashMap<PathBuf, Instant>,
    /// The selected stream is parked: its tile is offscreen and no modal
    /// shows it, so frames aren't drained (bounded backpressure stalls
    /// the decoder) and `animating` ignores the lane (P0.4).
    sel_parked: bool,
    /// Set on `[`/`]` — the skip flash bar shows for a moment after.
    skip_flash_at: Option<Instant>,
    /// Skip timer (`toggle_skip_timer`): when armed, holds the arming
    /// instant so a clip already mid-play gets a full countdown from the
    /// toggle, not an instant advance. None = off.
    skip_timer_since: Option<Instant>,
    /// Clips already queued for meta.json healing this session (old
    /// cache entries lack pix_fmt, so live spawns fall back to the
    /// software chain until a background reprobe rewrites them — see
    /// `MediaService::request_reprobe`). Keeps repeat visits from
    /// re-queueing the same clip.
    reprobed: std::collections::HashSet<PathBuf>,
    sel_changed_at: Instant,
    hover_changed_at: Instant,
    demo: bool,
    /// CLI `--animation` override; beats the config's level when set.
    cli_animation: Option<AnimLevel>,
    /// Runtime sheet toggle (`a` key), ANDed with the level's sheets().
    anim_on: bool,
    /// Window focus state + the runtime toggle for pause-when-unfocused.
    focused: bool,
    focus_pause_on: bool,
    anim_grid: u32,
    atlas_cfg: AtlasCfg,
    /// Internal fullscreen-ish preview of the selected clip.
    quickview: bool,
    quickview_at: Instant,
    /// Fullview: the selected clip fills the whole window, letterboxed on
    /// black — no filmstrip/seekbar/blur. Reuses the selected clip's live
    /// hires stream (the same one the tile plays). Toggled by `fullview`.
    fullview: bool,
    /// The fullview chapter bar, while up or sliding (`chapter_mode`).
    chapters: Option<ChapterBar>,
    /// Chapter-probe answers per clip: `None` = probe in flight, `Some`
    /// = (real chapter starts, container duration). Fullview pre-warms
    /// an entry for the selected clip so the bar opens with its plan
    /// already in hand; chapters don't change, so entries live for the
    /// session.
    chapter_probe: HashMap<PathBuf, Option<ChapterPlan>>,
    /// Filmstrip slide position (in clip-index units), springing toward
    /// the selected index with the keyboard chase curve.
    strip_pos: f32,
    /// Where the strip is heading (clip-index units). Scroll/scrub gestures
    /// nudge this free-float target and `strip_pos` eases toward it, so wheel
    /// input flows like the grid instead of snapping frame-to-frame; when
    /// idle it homes on `selected`.
    strip_target: f32,
    /// Filmstrip chip under the cursor (quickview only) — scales up and
    /// gets the hover video lane, like grid hover.
    strip_hover: Option<usize>,
    /// Last time the pointer moved over the quickview video — reveals
    /// the seekbar, which fades out after `seekbar_hide_s` of stillness.
    seekbar_seen: Option<Instant>,
    /// Left button is down on the seekbar: keyframe seeks track the drag,
    /// one exact seek lands on release.
    scrubbing: bool,
    /// Background job counters for the progress indicator.
    jobs_total: u64,
    jobs_done: u64,
    jobs_finished_at: Option<Instant>,
    tuning: Tuning,
    keymap: KeyMap,
    /// None under `--no-config`: internal defaults, no hot-reload.
    tuning_file: Option<TuningFile>,
    selected: usize,
    hovered: Option<usize>,
    cursor: (f32, f32),
    scroll: f32,
    scroll_target: f32,
    scroll_vel: f32,
    zoom: f32,
    zoom_target: f32,
    /// Active camera chase strength: keyboard moves glide gentler than pans.
    chase: f32,
    /// The static grid snapshot drawn behind modal views (quickview's
    /// frosted backdrop): `(viewport w, viewport h, tiles)`, captured on
    /// the first modal frame and reused until every modal closes (or the
    /// window resizes). While it stands, no grid simulation runs at all.
    frozen_grid: Option<(f32, f32, Vec<Tile>)>,
    /// Previous frame's tiles + column count, for the reflow crossfade.
    last_cols: usize,
    last_tiles: Vec<Tile>,
    transition: Option<(Vec<Tile>, Instant)>,
    /// App start, the time base for looping micro-animations (loading dots).
    t0: Instant,
    /// Springs still in flight this frame (drives idle throttling).
    motion: bool,
    /// An anim-sheet frame was drawn this frame (grid is visibly cycling).
    anim_rendered: bool,
    /// Stay awake at least until this instant (input, fades, fresh uploads).
    wake_until: Instant,
    /// Render-loop wake handle; worker threads fire it (via `notify`)
    /// when they deliver work, so background completions repaint promptly
    /// without keeping the loop in continuous animation (P0.2).
    waker: Waker,
    /// `waker.wake()` as a plain closure, cloned into media workers and
    /// ingest reader threads.
    notify: ingest::Notify,
    /// Redraw-reason telemetry (debug log level only).
    redraw_stats: RedrawStats,
    last_scroll_event: Instant,
    viewport: Viewport,
    cmds: Vec<WindowCommand>,
    title: String,
}

/// Once-a-second debug log of how many frames ran and which condition
/// kept `Frame.animating` true — the acceptance instrument for idle-
/// throttling work (PERFORMANCE-TASKS.md P0.2): it distinguishes visual
/// animation, live playback, and explicit wake timers as redraw causes.
/// Negligible when debug logging is off (one `log_enabled!` check).
struct RedrawStats {
    at: Instant,
    frames: u32,
    idle: u32,
    motion: u32,
    sheets: u32,
    transition: u32,
    live: u32,
    timer: u32,
    /// High-water atlas slots occupied this second (P0.1 — the measured
    /// side of P0.5's atlas sizing).
    slots_used: usize,
    /// High-water visible+prefetch slot demand (statics + anims capped
    /// by library size, + live/hover lanes).
    slots_demand: usize,
}

impl RedrawStats {
    fn new() -> Self {
        Self {
            at: Instant::now(),
            frames: 0,
            idle: 0,
            motion: 0,
            sheets: 0,
            transition: 0,
            live: 0,
            timer: 0,
            slots_used: 0,
            slots_demand: 0,
        }
    }

    fn slots(&mut self, used: usize, demand: usize) {
        self.slots_used = self.slots_used.max(used);
        self.slots_demand = self.slots_demand.max(demand);
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        motion: bool,
        sheets: bool,
        transition: bool,
        live: bool,
        timer: bool,
        animating: bool,
    ) {
        if !log::log_enabled!(log::Level::Debug) {
            return;
        }
        self.frames += 1;
        self.idle += u32::from(!animating);
        self.motion += u32::from(motion);
        self.sheets += u32::from(sheets);
        self.transition += u32::from(transition);
        self.live += u32::from(live);
        self.timer += u32::from(timer);
        if self.at.elapsed() >= Duration::from_secs(1) {
            log::debug!(
                "redraw: {} frames/s ({} idle; causes: motion {} sheets {} transition {} live {} timer {}); atlas slots used {} / zone demand {}",
                self.frames,
                self.idle,
                self.motion,
                self.sheets,
                self.transition,
                self.live,
                self.timer,
                self.slots_used,
                self.slots_demand,
            );
            *self = Self::new();
        }
    }
}

impl Default for Switchblade {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot config load (same search order as the app) for CLI verbs
/// that need the current tuning without starting the app.
pub fn load_tuning() -> Tuning {
    TuningFile::new(tuning::config_path())
        .poll()
        .map(|cfg| cfg.tuning)
        .unwrap_or_default()
}

/// The media recipe the given tuning produces — the SAME clamps as app
/// startup, so cache maintenance agrees with the app about which
/// artifacts the current configuration serves.
pub fn recipe_from(tuning: &Tuning) -> Recipe {
    Recipe {
        thumb_w: tuning.thumb_width.clamp(64, 2048),
        thumb_h: tuning.thumb_height.clamp(36, 2048),
        // Config quality is 1..10 (10 = best); ffmpeg -q:v wants
        // 2..31 (2 = best). 12 - q spans 2..11 — even quality 1
        // avoids the hideous end of the scale.
        quality: 12 - tuning.thumb_quality.clamp(1, 10),
        anim_grid: tuning.anim_grid.clamp(1, 4),
    }
}

impl Switchblade {
    pub fn new() -> Self {
        Self::with_options(Options::default())
    }

    pub fn with_options(opts: Options) -> Self {
        // Load config up front: atlas geometry, the media recipe, and the
        // ingest recurse flag are startup-only (the rest keeps
        // hot-reloading per frame). `--no-config` skips the file entirely
        // — internal defaults, nothing watched.
        let mut tuning_file = (!opts.no_config).then(|| TuningFile::new(tuning::config_path()));
        let (tuning, keymap) = match tuning_file.as_mut().and_then(|f| f.poll()) {
            Some(cfg) => (cfg.tuning, cfg.keymap),
            None => (Tuning::default(), KeyMap::default()),
        };
        // Startup-only, before any cache access (ingest stats, thumb
        // requests): the fingerprint keying can't change under a live
        // cache without splitting entries across two keyings.
        sb_media::set_cache_key(tuning.cache_key);

        // Worker threads (ingest, media) fire this after delivering work;
        // the window layer arms it and turns each burst into one redraw.
        let waker = Waker::new();
        let notify: ingest::Notify = {
            let w = waker.clone();
            std::sync::Arc::new(move || w.wake())
        };

        // CLI paths beat stdin; --demo beats both; a TTY stdin with
        // neither also falls back to the demo grid.
        let rx = if !opts.inputs.is_empty() && !opts.demo {
            Some(ingest::spawn_args_reader(
                opts.inputs.clone(),
                tuning.recurse,
                notify.clone(),
            ))
        } else if opts.demo {
            None
        } else {
            ingest::spawn_stdin_reader(tuning.recurse, notify.clone())
        };
        let demo = rx.is_none();
        let recipe = recipe_from(&tuning);
        let (slot_w, slot_h) = (recipe.thumb_w, recipe.thumb_h);
        let atlas_cfg = AtlasCfg {
            slot_w,
            slot_h,
            cols: (tuning.atlas_width.min(16384) / slot_w).max(1),
            rows: (tuning.atlas_height.min(16384) / slot_h).max(1),
            hires_w: tuning.quickview_max_width.clamp(320, 4096),
            hires_h: tuning.quickview_max_height.clamp(180, 4096),
        };
        log::info!(
            "atlas: {}x{} slots of {slot_w}x{slot_h} ({} MB)",
            atlas_cfg.cols,
            atlas_cfg.rows,
            atlas_cfg.tex_w() as u64 * atlas_cfg.tex_h() as u64 * 4 / (1024 * 1024)
        );
        let mut app = Self {
            clips: Vec::new(),
            index: HashMap::new(),
            rx,
            media: MediaService::new(recipe, notify.clone()),
            slots: vec![None; atlas_cfg.slots()],
            live_sel: None,
            live_hover: None,
            warm: Vec::new(),
            hires_frame: None,
            hires_shown: None,
            pending_reselect: None,
            live_retry: HashMap::new(),
            sel_parked: false,
            skip_flash_at: None,
            skip_timer_since: None,
            reprobed: std::collections::HashSet::new(),
            sel_changed_at: Instant::now(),
            hover_changed_at: Instant::now(),
            demo,
            cli_animation: opts.animation,
            anim_on: true,
            focused: true,
            focus_pause_on: true,
            anim_grid: recipe.anim_grid,
            atlas_cfg,
            quickview: false,
            quickview_at: Instant::now(),
            fullview: false,
            chapters: None,
            chapter_probe: HashMap::new(),
            strip_pos: 0.0,
            strip_target: 0.0,
            strip_hover: None,
            seekbar_seen: None,
            scrubbing: false,
            jobs_total: 0,
            jobs_done: 0,
            jobs_finished_at: None,
            tuning,
            keymap,
            tuning_file,
            selected: 0,
            hovered: None,
            cursor: (0.0, 0.0),
            scroll: 0.0,
            scroll_target: 0.0,
            scroll_vel: 0.0,
            zoom: 1.0,
            zoom_target: 1.0,
            chase: 0.22,
            frozen_grid: None,
            last_cols: 0,
            last_tiles: Vec::new(),
            transition: None,
            t0: Instant::now(),
            motion: true,
            anim_rendered: false,
            wake_until: Instant::now() + Duration::from_secs(1),
            waker,
            notify,
            redraw_stats: RedrawStats::new(),
            last_scroll_event: Instant::now(),
            viewport: Viewport {
                width: 1280.0,
                height: 800.0,
            },
            cmds: Vec::new(),
            title: String::new(),
        };
        // Queued now, drained by the window layer right after the window
        // exists — so --fullscreen never flashes a windowed frame.
        if let Some(fast) = opts.fullscreen {
            app.cmds.push(WindowCommand::ToggleFullscreen { fast });
        }
        if demo {
            log::info!("stdin is a tty — demo mode with {DEMO_TILES} fake tiles");
            let now = Instant::now();
            for i in 0..DEMO_TILES {
                app.clips.push(Clip {
                    path: PathBuf::from(format!("demo/clip_{i:04}.mp4")),
                    readable: true,
                    cloud: false,
                    cached: true, // fake tiles cost nothing to land on
                    // Staggered spawn cascades the fade-in across the field.
                    spawned: now + Duration::from_millis(i as u64 * 2),
                    scale: 1.0,
                    emph: 0.0,
                    thumb: Thumb::Failed, // demo paths aren't real; never request
                    anim: Thumb::Failed,
                });
            }
        }
        app
    }

    // --- layout ---

    /// Grid layout derived from tuning + viewport + zoom. `tuning.tile_width`
    /// × zoom is the *ideal* width used to choose the column count; tiles
    /// then stretch so the columns exactly fill the viewport and the
    /// background barely shows.
    fn layout(&self) -> Layout {
        self.layout_with(self.zoom)
    }

    fn layout_with(&self, zoom: f32) -> Layout {
        let t = &self.tuning;
        let ideal = (t.tile_width * zoom).max(40.0);
        let cols = (((self.viewport.width - t.gap) / (ideal + t.gap)).floor() as usize).max(1);
        let tile_w = ((self.viewport.width - t.gap * (cols as f32 + 1.0)) / cols as f32).max(1.0);
        let tile_h = tile_w * t.tile_height / t.tile_width.max(1.0);
        Layout {
            cols,
            tile_w,
            tile_h,
            cell_w: tile_w + t.gap,
            cell_h: tile_h + t.gap,
        }
    }

    fn rows(&self, lay: &Layout) -> usize {
        self.clips.len().div_ceil(lay.cols)
    }

    fn cell_origin(&self, lay: &Layout, col: usize, row: usize) -> (f32, f32) {
        let g = self.tuning.gap;
        (
            g + col as f32 * lay.cell_w,
            g + row as f32 * lay.cell_h - self.scroll,
        )
    }

    fn content_height(&self, lay: &Layout) -> f32 {
        self.tuning.gap + self.rows(lay) as f32 * lay.cell_h
    }

    fn max_scroll(&self, lay: &Layout) -> f32 {
        (self.content_height(lay) - self.viewport.height).max(0.0)
    }

    fn tile_at(&self, lay: &Layout, x: f32, y: f32) -> Option<usize> {
        let g = self.tuning.gap;
        let xx = x - g;
        let yy = y + self.scroll - g;
        if xx < 0.0 || yy < 0.0 {
            return None;
        }
        let col = (xx / lay.cell_w) as usize;
        let row = (yy / lay.cell_h) as usize;
        if col >= lay.cols || xx % lay.cell_w > lay.tile_w || yy % lay.cell_h > lay.tile_h {
            return None;
        }
        let i = row * lay.cols + col;
        (i < self.clips.len()).then_some(i)
    }

    // --- selection ---

    fn move_selection(&mut self, dx: i32, dy: i32) {
        if self.clips.is_empty() {
            return;
        }
        let lay = self.layout();
        let sel = self.selected as i32;
        let last = self.clips.len() as i32 - 1;
        let idx = if dy == 0 {
            // Horizontal moves are linear: right at the row's end wraps to
            // the next row's first chip (the "next" clip in stdin order).
            (sel + dx).clamp(0, last) as usize
        } else {
            let cols = lay.cols as i32;
            let rows = self.rows(&lay) as i32;
            let col = (sel % cols).clamp(0, cols - 1);
            let row = (sel / cols + dy).clamp(0, rows - 1);
            (row * cols + col).min(last) as usize
        };
        if idx != self.selected {
            self.selected = idx;
            self.sel_changed_at = Instant::now();
            // An explicit move outranks the D swap's pending reselect.
            self.pending_reselect = None;
        }
        self.scroll_to_selected();
    }

    /// Smoothly bring the selected row toward the vertical center. Uses the
    /// gentler key-move chase curve so whole-screen jumps glide, not jolt.
    fn scroll_to_selected(&mut self) {
        let lay = self.layout();
        let row = self.selected / lay.cols;
        let row_center = self.tuning.gap + row as f32 * lay.cell_h + lay.tile_h * 0.5;
        self.scroll_target =
            (row_center - self.viewport.height * 0.5).clamp(0.0, self.max_scroll(&lay));
        self.scroll_vel = 0.0;
        self.chase = self.tuning.key_snap_strength;
    }

    /// Move the selection to an arbitrary index (random jump, skip-timer
    /// advance) — the same bookkeeping as `move_selection`, plus a
    /// filmstrip snap on far jumps: sliding the strip across half the
    /// library reads as a smear, single steps still glide.
    fn select_index(&mut self, idx: usize) {
        if idx >= self.clips.len() {
            return;
        }
        if idx != self.selected {
            self.selected = idx;
            self.sel_changed_at = Instant::now();
            self.pending_reselect = None;
        }
        self.scroll_to_selected();
        if self.quickview && (idx as f32 - self.strip_pos).abs() > 4.0 {
            self.strip_pos = idx as f32;
            self.strip_target = self.strip_pos;
        }
        // Not always an input event (the skip timer calls this): keep the
        // loop awake through the settle timers, like event() does.
        self.wake(0.6);
    }

    /// `jump_random`: select a uniformly random other clip — but only
    /// among clips whose thumbnail is already in the disk cache, so a
    /// jump into unswept territory can't queue a screenful of on-demand
    /// ffmpeg work (the gen sweep widens the candidate pool as it runs).
    fn jump_random(&mut self) {
        let candidates: Vec<usize> = (0..self.clips.len())
            .filter(|&i| i != self.selected && self.clips[i].cached)
            .collect();
        if candidates.is_empty() {
            log::info!("jump_random: no cached clips to jump to yet");
            return;
        }
        let mut s = rng_seed();
        let idx = candidates[(next_rand(&mut s) % candidates.len() as u64) as usize];
        self.select_index(idx);
    }

    /// `shuffle_library`: Fisher–Yates the cached clips to the front of
    /// the grid (uncached ones keep their order after them). All per-clip
    /// state rides inside `Clip` and moves with it; everything keyed by
    /// clip *index* — the path→index map, atlas slot owners, the live/
    /// warm/hover lanes, the selection — is remapped through the
    /// permutation, so the selected clip keeps playing and lands
    /// somewhere new. Hover clears (the pointer is over a different clip
    /// now); mid-ingest arrivals simply append after the shuffled block.
    fn shuffle_library(&mut self) {
        let n = self.clips.len();
        if n < 2 {
            return;
        }
        // Only cached clips shuffle; they move to the FRONT of the
        // library and the unswept remainder follows in its original
        // order. Shuffling uncached clips onto the visible screen would
        // queue a top-priority gen job per tile — the cached-first
        // partition keeps the viewport serving from disk.
        let mut cached: Vec<usize> = (0..n).filter(|&i| self.clips[i].cached).collect();
        if cached.len() < 2 {
            log::info!("shuffle_library: fewer than two cached clips — nothing to shuffle");
            return;
        }
        let mut s = rng_seed();
        for i in (1..cached.len()).rev() {
            let j = (next_rand(&mut s) % (i as u64 + 1)) as usize;
            cached.swap(i, j);
        }
        let mut order = cached; // order[new] = old
        order.extend((0..n).filter(|&i| !self.clips[i].cached));
        let mut new_of_old = vec![0usize; n];
        for (new, &old) in order.iter().enumerate() {
            new_of_old[old] = new;
        }
        let mut old: Vec<Option<Clip>> = self.clips.drain(..).map(Some).collect();
        self.clips = order.iter().map(|&o| old[o].take().unwrap()).collect();
        let remap = |i: &mut usize| {
            if let Some(&ni) = new_of_old.get(*i) {
                *i = ni;
            }
        };
        self.index.values_mut().for_each(remap);
        for slot in self.slots.iter_mut().flatten() {
            remap(&mut slot.0);
        }
        if let Some(l) = &mut self.live_sel {
            remap(&mut l.clip);
        }
        if let Some(h) = &mut self.live_hover {
            remap(&mut h.clip);
        }
        for w in &mut self.warm {
            remap(&mut w.clip);
        }
        remap(&mut self.selected);
        self.hovered = None;
        self.strip_hover = None;
        if self.quickview {
            self.strip_pos = self.selected as f32;
            self.strip_target = self.strip_pos;
        }
        // Crossfade the old arrangement out, like the zoom reflow / D swap.
        if !self.last_tiles.is_empty() {
            self.transition = Some((std::mem::take(&mut self.last_tiles), Instant::now()));
        }
        self.scroll_to_selected();
        self.wake(1.0);
    }

    /// Skip timer: with the timer armed, advance to the next clip
    /// (wrapping) once the current one has played `skip_timer_s`. "Played"
    /// means live frames on screen — the countdown starts at first frame,
    /// not at selection, so cold-spawn latency doesn't eat watch time. At
    /// levels without live video it counts from the selection change; a
    /// stream that never delivers gets `SKIP_TIMER_SPAWN_GRACE_S` before
    /// the slideshow moves on anyway.
    fn tick_skip_timer(&mut self) {
        let Some(since) = self.skip_timer_since else {
            return;
        };
        // Paused, parked (user scrolled away), scrubbing, or mid-swap:
        // hold the countdown rather than yank the view around.
        if self.clips.is_empty()
            || self.paused()
            || self.sel_parked
            || self.scrubbing
            || self.pending_reselect.is_some()
            || self.chapters.is_some()
        {
            return;
        }
        let limit = Duration::from_secs_f32(self.tuning.skip_timer_s.max(0.05));
        let first_frame = self
            .live_sel
            .as_ref()
            .filter(|l| l.clip == self.selected)
            .and_then(|l| l.first_frame);
        let due = match first_frame {
            Some(ff) => ff.max(since).elapsed() >= limit,
            None if !self.level().live() => self.sel_changed_at.max(since).elapsed() >= limit,
            None => {
                self.sel_changed_at.max(since).elapsed()
                    >= limit + Duration::from_secs_f32(SKIP_TIMER_SPAWN_GRACE_S)
            }
        };
        if due {
            let next = (self.selected + 1) % self.clips.len();
            if next != self.selected {
                self.select_index(next);
            }
        }
    }

    // --- input ---

    fn key(&mut self, key: Key) {
        // Movement keys are reserved; everything else goes through the keymap.
        match key {
            // Esc peels layers: the chapter bar slides down first, then
            // fullview exits, then quickview.
            Key::Escape if self.chapters.as_ref().is_some_and(|b| b.open) => {
                return self.close_chapter_bar();
            }
            Key::Escape if self.fullview => {
                self.fullview = false;
                return;
            }
            Key::Escape if self.quickview => {
                self.quickview = false;
                return;
            }
            Key::Char('h') | Key::Left => return self.move_selection(-1, 0),
            Key::Char('l') | Key::Right => return self.move_selection(1, 0),
            Key::Char('k') | Key::Up => return self.move_selection(0, -1),
            Key::Char('j') | Key::Down => return self.move_selection(0, 1),
            _ => {}
        }
        let Some(action) = self.keymap.action_for(&key) else {
            return;
        };
        match action {
            Action::Quit => self.cmds.push(WindowCommand::Quit),
            Action::ToggleFullscreen { fast } => {
                self.cmds.push(WindowCommand::ToggleFullscreen { fast })
            }
            Action::ZoomIn => self.set_zoom(self.zoom_target * 1.15),
            Action::ZoomOut => self.set_zoom(self.zoom_target / 1.15),
            Action::ZoomReset => self.set_zoom(1.0),
            Action::ToggleAnim => {
                self.anim_on = !self.anim_on;
                log::info!(
                    "background animation {}",
                    if self.anim_on { "on" } else { "off" }
                );
            }
            Action::ToggleFocusPause => {
                self.focus_pause_on = !self.focus_pause_on;
                log::info!(
                    "pause-when-unfocused {}",
                    if self.focus_pause_on { "on" } else { "off" }
                );
            }
            Action::Quickview => {
                self.quickview = !self.quickview;
                if self.quickview {
                    self.quickview_at = Instant::now();
                    self.strip_pos = self.selected as f32;
                    self.strip_target = self.strip_pos;
                }
            }
            Action::Fullview => self.fullview = !self.fullview,
            Action::ChapterMode => self.toggle_chapters(),
            Action::Skip { forward, amount } => self.skip(forward, amount),
            Action::OpenParent => self.open_parent(),
            Action::JumpRandom => self.jump_random(),
            Action::ShuffleLibrary => self.shuffle_library(),
            Action::ToggleSkipTimer => {
                self.skip_timer_since = match self.skip_timer_since {
                    Some(_) => None,
                    None => Some(Instant::now()),
                };
                log::info!(
                    "skip timer {} ({}s per clip)",
                    if self.skip_timer_since.is_some() {
                        "on"
                    } else {
                        "off"
                    },
                    self.tuning.skip_timer_s
                );
            }
            Action::CopyPath => {
                if let Some(clip) = self.clips.get(self.selected) {
                    commands::copy_path(&clip.path);
                }
            }
            Action::Spawn { program, args } => {
                if let Some(clip) = self.clips.get(self.selected) {
                    if clip.cloud {
                        log::info!(
                            "{} is an iCloud placeholder — opening it will trigger a download",
                            clip.path.display()
                        );
                    }
                    commands::spawn_external(&program, &args, &clip.path);
                }
            }
        }
    }

    /// `[`/`]`: jump the playing clip by a fraction of its duration — an
    /// in-place `seek()` on the resident decoder (PLAN.md §15). The
    /// demuxer jumps and the decoder flushes; the last frame stays on
    /// screen (the hires texture still holds it) until the new position
    /// delivers, so it reads as freeze-then-jump — GOP-bound (~30–600ms)
    /// instead of the old ~1s respawn floor, and chained presses need no
    /// checkpoint machinery. Exact seeks: honest landings even on
    /// sparse-keyframe sources. Wraps at the ends — playback loops anyway.
    fn skip(&mut self, forward: bool, amount: Option<f32>) {
        let Some(path) = self.clips.get(self.selected).map(|c| c.path.clone()) else {
            return;
        };
        let Some(l) = &self.live_sel else { return };
        if l.path != path {
            return; // stream not on the selected clip (e.g. mid-swap)
        }
        let Some(d) = l.duration.filter(|d| *d > 0.05) else {
            return; // duration unknown: no meaningful fraction to jump
        };
        let frac = amount
            .unwrap_or(self.tuning.skip_fraction)
            .clamp(0.001, 1.0) as f64;
        let delta = if forward { frac * d } else { -frac * d };
        l.player.seek((l.position() + delta).rem_euclid(d), true);
        self.skip_flash_at = Some(Instant::now());
        self.wake(1.5); // outlive the flash bar's hold + fade
    }

    /// Played fraction (0..1) of the selected clip's live stream — the
    /// decoder's real position (or the in-flight seek target), only when
    /// the stream is on the selected clip and its duration is known.
    fn seekbar_pos(&self) -> Option<f32> {
        let l = self.live_sel.as_ref()?;
        if self
            .clips
            .get(self.selected)
            .is_none_or(|c| c.path != l.path)
        {
            return None;
        }
        let d = l.duration.filter(|d| *d > 0.05)?;
        Some(((l.position().rem_euclid(d) / d) as f32).clamp(0.0, 1.0))
    }

    /// The post-skip flash bar: `Some((played fraction, alpha))` while
    /// it's visible — holds a second after a skip, then fades out fast.
    fn skip_bar(&self) -> Option<(f32, f32)> {
        let t = self.skip_flash_at?.elapsed().as_secs_f32();
        if t >= SKIP_BAR_HOLD_S + SKIP_BAR_FADE_S {
            return None;
        }
        // Level "none": fades complete in one frame — show, then vanish.
        let alpha = if self.level().ui() {
            ((SKIP_BAR_HOLD_S + SKIP_BAR_FADE_S - t) / SKIP_BAR_FADE_S).min(1.0)
        } else if t < SKIP_BAR_HOLD_S {
            1.0
        } else {
            return None;
        };
        Some((self.seekbar_pos()?, alpha))
    }

    /// Pointer-reveal alpha for the quickview seekbar: solid while the
    /// cursor moved over the video within `seekbar_hide_s`, then fading
    /// over `seekbar_fade_ms`. Scrubbing pins it visible.
    fn seekbar_alpha(&self) -> f32 {
        if self.scrubbing {
            return 1.0;
        }
        let Some(seen) = self.seekbar_seen else {
            return 0.0;
        };
        let t = seen.elapsed().as_secs_f32();
        let hide = self.tuning.seekbar_hide_s.max(0.0);
        if t <= hide {
            return 1.0;
        }
        if !self.level().ui() {
            return 0.0; // snap mode: no fade tail
        }
        let fade = (self.tuning.seekbar_fade_ms / 1000.0).max(0.001);
        (1.0 - (t - hide) / fade).clamp(0.0, 1.0)
    }

    /// UV + pixel dims + `hires` flag for the selected clip's current
    /// frame: the live hires stream when it's up for this clip (already
    /// running, already sharp — no handoff), else the static thumb.
    /// Shared by the quickview modal and fullview so they can't drift.
    fn selected_video_src(&self, clip: &Clip) -> Option<([f32; 4], f32, f32, bool)> {
        let live = self.live_sel.as_ref().filter(|l| {
            (l.path == clip.path || self.pending_reselect.as_ref() == Some(&l.path))
                && (l.first_frame.is_some() || self.hires_shown.as_ref() == Some(&l.path))
        });
        if let Some(l) = live {
            let (w, h) = (l.player.w as f32, l.player.h as f32);
            let (hw, hh) = (self.atlas_cfg.hires_w as f32, self.atlas_cfg.hires_h as f32);
            Some((
                [0.5 / hw, 0.5 / hh, (w - 1.0) / hw, (h - 1.0) / hh],
                w,
                h,
                true,
            ))
        } else {
            match clip.thumb {
                Thumb::Ready { slot, tw, th, .. } => Some((
                    self.atlas_cfg.uv(slot, 0.0, 0.0, tw as f32, th as f32),
                    tw as f32,
                    th as f32,
                    false,
                )),
                _ => None,
            }
        }
    }

    /// The quickview modal's video rectangle — the same geometry
    /// `build_frame` draws, factored out so pointer hit-tests and the
    /// seekbar can't drift from what's on screen.
    fn quickview_video_rect(&self) -> Option<(f32, f32, f32, f32)> {
        if !self.quickview {
            return None;
        }
        let clip = self.clips.get(self.selected)?;
        let live = self.live_sel.as_ref().filter(|l| {
            (l.path == clip.path || self.pending_reselect.as_ref() == Some(&l.path))
                && (l.first_frame.is_some() || self.hires_shown.as_ref() == Some(&l.path))
        });
        let (tw, th) = match live {
            Some(l) => (l.player.w as f32, l.player.h as f32),
            None => match clip.thumb {
                Thumb::Ready { tw, th, .. } => (tw as f32, th as f32),
                _ => return None,
            },
        };
        let vw = self.viewport.width;
        let (_, _, strip_y) = self.strip_geom();
        let avail_h = (strip_y - 18.0).max(60.0);
        let a = tw / th.max(1.0);
        let (mut w, mut h) = (vw * 0.88, avail_h * 0.92);
        if a > w / h {
            h = w / a;
        } else {
            w = h * a;
        }
        Some(((vw - w) * 0.5, (avail_h - h) * 0.5 + 6.0, w, h))
    }

    /// Letterboxed video rect for fullview — the same fit `build_frame`
    /// draws, factored out so the seekbar geometry can't drift from it.
    fn fullview_video_rect(&self) -> Option<(f32, f32, f32, f32)> {
        if !self.fullview {
            return None;
        }
        let clip = self.clips.get(self.selected)?;
        let (_, tw, th, _) = self.selected_video_src(clip)?;
        let (vw, vh) = (self.viewport.width, self.viewport.height);
        let a = tw / th.max(1.0);
        let (mut w, mut h) = (vw, vh);
        if a > vw / vh.max(1.0) {
            h = w / a;
        } else {
            w = h * a;
        }
        Some(((vw - w) * 0.5, (vh - h) * 0.5, w, h))
    }

    /// Video rect for whichever full-bleed mode owns the seekbar. Fullview
    /// wins — it draws on top of any quickview underneath.
    fn active_video_rect(&self) -> Option<(f32, f32, f32, f32)> {
        if self.fullview {
            self.fullview_video_rect()
        } else {
            self.quickview_video_rect()
        }
    }

    /// Seekbar line geometry: (left x, width, bottom y). The bar floats
    /// inset from the video's edges (side padding scales with the frame),
    /// shared by the quickview modal and fullview.
    fn seekbar_line(&self) -> Option<(f32, f32, f32)> {
        let (x, y, w, h) = self.active_video_rect()?;
        let pad_x = (w * 0.05).clamp(16.0, 48.0);
        let pad_y = (h * 0.05).clamp(14.0, 32.0);
        Some((x + pad_x, (w - pad_x * 2.0).max(8.0), y + h - pad_y))
    }

    /// Fraction of the bar under screen-x (unclamped hit — used while
    /// dragging, where the pointer may wander off the line).
    fn seekbar_frac(&self, x: f32) -> Option<f32> {
        let (bx, bw, _) = self.seekbar_line()?;
        Some(((x - bx) / bw).clamp(0.0, 1.0))
    }

    /// Fraction under the pointer when it's on the bar's hit band — a
    /// taller target than the drawn line, so the bar is easy to grab.
    fn seekbar_hit(&self, x: f32, y: f32) -> Option<f32> {
        let (bx, bw, bot) = self.seekbar_line()?;
        (self.seekbar_pos().is_some()
            && (bot - 18.0..=bot + 10.0).contains(&y)
            && (bx - 6.0..=bx + bw + 6.0).contains(&x))
        .then(|| ((x - bx) / bw).clamp(0.0, 1.0))
    }

    /// Draws the seekbar (track + fill + hover storyboard) inset from the
    /// bottom of the video rect `(rx, .., rw, ..)`. Shared by the quickview
    /// modal and fullview so the two can't drift. The fill tracks the
    /// decoder's real position — the seek target while one is in flight, so
    /// scrubbing feels attached to the pointer even mid-decode.
    fn push_seekbar(&self, tiles: &mut Vec<Tile>, rx: f32, rw: f32, clip: &Clip, bar_a: f32) {
        let t = &self.tuning;
        if bar_a <= 0.005 {
            return;
        }
        let (Some(pos), Some((bx, bw, bot))) = (self.seekbar_pos(), self.seekbar_line()) else {
            return;
        };
        let (cx, cy) = self.cursor;
        let hot = self.scrubbing || self.seekbar_hit(cx, cy).is_some();
        let bh = if hot {
            t.seekbar_hover_height
        } else {
            t.seekbar_height
        }
        .max(2.0);
        let bar = |x: f32, w: f32, color: [f32; 4]| Tile {
            x,
            y: bot - bh,
            w,
            h: bh,
            color,
            border_color: [0.0; 4],
            corner_radius: bh * 0.5,
            border_width: 0.0,
            uv: [0.0; 4],
            uv2: [0.0; 4],
            frame_fade: 0.0,
            tex_mix: 0.0,
            hires: false,
        };
        // Track: a translucent-white tint (reads as a bright frosted line
        // over the video, not a grey slab); the fill is solid white.
        tiles.push(bar(bx, bw, [1.0, 1.0, 1.0, 0.28 * bar_a]));
        tiles.push(bar(bx, (bw * pos).max(bh), [1.0, 1.0, 1.0, 0.95 * bar_a]));
        // Storyboard preview (PLAN.md §14 M8 phase 1): the anim sheet is
        // already g² frames spread across the duration — hovering the bar
        // shows the cell nearest that timestamp. A denser dedicated strip
        // lands later if g² is too coarse.
        let hover_f = if self.scrubbing {
            self.seekbar_frac(cx)
        } else {
            self.seekbar_hit(cx, cy)
        };
        if let (Some(f), &Thumb::Ready { slot, tw, th, .. }) = (hover_f, &clip.anim) {
            let tw_px = t.seekbar_thumb_width;
            if tw_px >= 8.0 {
                let g = self.anim_grid.max(1) as usize;
                let cells = g * g;
                let k = ((f * cells as f32) as usize).min(cells - 1);
                let (fw, fh) = (tw as f32 / g as f32, th as f32 / g as f32);
                let cuv = self
                    .atlas_cfg
                    .uv(slot, (k % g) as f32 * fw, (k / g) as f32 * fh, fw, fh);
                let cw2 = tw_px.min(rw * 0.5);
                let ch2 = cw2 * fh / fw.max(1.0);
                tiles.push(Tile {
                    x: (bx + f * bw - cw2 * 0.5).clamp(rx + 4.0, rx + rw - cw2 - 4.0),
                    y: bot - bh - ch2 - 10.0,
                    w: cw2,
                    h: ch2,
                    color: [0.02, 0.02, 0.03, bar_a],
                    border_color: [0.85, 0.85, 0.9, 0.7 * bar_a],
                    corner_radius: 4.0,
                    border_width: 1.0,
                    uv: cuv,
                    uv2: [0.0; 4],
                    frame_fade: 0.0,
                    tex_mix: bar_a,
                    hires: false,
                });
            }
        }
    }

    /// Seek the selected clip's stream to a fraction of its duration.
    /// Keyframe mode while a drag is in flight (instant feedback), exact
    /// on release / click-settle (PLAN.md §14 M8 two-phase scrub).
    fn scrub_seek(&mut self, frac: f32, exact: bool) {
        let Some(clip) = self.clips.get(self.selected) else {
            return;
        };
        let Some(l) = &self.live_sel else { return };
        if l.path != clip.path {
            return;
        }
        let Some(d) = l.duration.filter(|d| *d > 0.05) else {
            return;
        };
        // Stay a frame short of the end: an exact seek to the tail would
        // land on EOF and wrap the loop back to 0:00.
        l.player
            .seek((frac as f64 * d).clamp(0.0, (d - 0.05).max(0.0)), exact);
        self.wake(1.5);
    }

    /// `D`: rebuild the library from the selected clip's parent directory
    /// (its siblings, non-recursive), streaming in the background like any
    /// other source. The selected clip's live stream survives the swap —
    /// `pending_reselect` shields it until its path streams back in and
    /// becomes the selection again.
    fn open_parent(&mut self) {
        if self.demo || self.pending_reselect.is_some() {
            return;
        }
        let Some(clip) = self.clips.get(self.selected) else {
            return;
        };
        let path = clip.path.clone();
        let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) else {
            return;
        };
        log::info!("browsing siblings of {}", path.display());
        self.rx = Some(ingest::spawn_dir_reader(
            dir.to_path_buf(),
            self.notify.clone(),
        ));
        // All per-clip state is index-keyed; drop it and let the new
        // listing stream in (thumbs re-serve from the disk cache).
        self.chapters = None; // the bar's clip context is gone
        self.clips.clear();
        self.index.clear();
        self.slots.fill(None);
        self.live_hover = None;
        self.warm.clear();
        self.hovered = None;
        self.strip_hover = None;
        self.selected = 0;
        self.pending_reselect = Some(path);
        // Fade the old grid out over the new one, like the zoom reflow.
        if !self.last_tiles.is_empty() {
            self.transition = Some((std::mem::take(&mut self.last_tiles), Instant::now()));
        }
        self.wake(1.0);
    }

    /// `chapter_mode` (g): toggle the chapter bar — a fullview add-on.
    /// Opening from anywhere enters fullview first. Fullview pre-warms
    /// the probe (and the anim sheet), so the plan is usually cached and
    /// the bar opens fully populated; a cold open waits one cheap
    /// ffprobe. Chip images come from the clip's anim sheet, requested
    /// on demand like the seekbar storyboard.
    fn toggle_chapters(&mut self) {
        if self.chapters.as_ref().is_some_and(|b| b.open) {
            return self.close_chapter_bar();
        }
        let path = match self.clips.get(self.selected) {
            Some(c) if !self.demo && c.readable && !c.cloud => c.path.clone(),
            _ => return,
        };
        self.fullview = true;
        let duration = self
            .live_sel
            .as_ref()
            .filter(|l| l.path == path)
            .and_then(|l| l.duration);
        self.chapters = Some(ChapterBar {
            path: path.clone(),
            duration,
            times: None,
            open: true,
            slide: 0.0,
            pos: 0.0,
            target: 0.0,
        });
        match self.chapter_probe.get(&path).cloned() {
            // Pre-warmed answer: the plan installs this same tick.
            Some(Some((times, d))) => self.apply_chapter_plan(&path, &times, d),
            Some(None) => {} // probe already in flight
            None => {
                self.chapter_probe.insert(path.clone(), None);
                self.media.request_chapters(path);
            }
        }
        self.wake(0.8);
    }

    /// Install a probe answer into the open bar: real chapters win, a
    /// chapterless clip gets checkpoints synthesized from its duration,
    /// and a too-short chapterless clip sends the bar back down. The
    /// strip starts centered on the chapter that's already playing.
    fn apply_chapter_plan(&mut self, path: &std::path::Path, times: &[f64], duration: Option<f64>) {
        let applies = self
            .chapters
            .as_ref()
            .is_some_and(|b| b.path == path && b.times.is_none());
        if !applies {
            return;
        }
        let d = duration
            .or(self.chapters.as_ref().unwrap().duration)
            .filter(|d| *d > 0.05);
        let times: Vec<f64> = if times.len() >= 2 {
            times.to_vec()
        } else {
            d.map(|d| {
                let n = checkpoint_count(d);
                (0..n).map(|k| d * k as f64 / n.max(1) as f64).collect()
            })
            .unwrap_or_default()
        };
        if times.is_empty() {
            log::info!(
                "no chapters (and under a minute) — chapter bar hidden: {}",
                path.display()
            );
            self.close_chapter_bar();
        } else {
            let (lo, hi) = self.chapter_pos_bounds(times.len());
            let b = self.chapters.as_mut().unwrap();
            b.duration = d.or(b.duration);
            b.times = Some(times);
            b.pos = 0.0f32.clamp(lo, hi);
            b.target = b.pos;
            let bar = self.chapters.as_ref().unwrap().clone();
            if let Some(cur) = self.current_chapter(&bar) {
                let b = self.chapters.as_mut().unwrap();
                b.pos = (cur as f32).clamp(lo, hi);
                b.target = b.pos;
            }
        }
        self.wake(0.8);
    }

    /// Slide the chapter bar down; its state drops once the slide lands.
    fn close_chapter_bar(&mut self) {
        if let Some(b) = &mut self.chapters
            && b.open
        {
            b.open = false;
            self.wake(0.8);
        }
    }

    /// Chapter chip width: the strip's fixed height at the SELECTED
    /// clip's true aspect — every chapter chip shares the clip's shape
    /// (portrait clips get narrower chips, never taller ones).
    fn chapter_chip_w(&self) -> f32 {
        let (_, ch, _) = self.strip_geom();
        ch * self.chip_aspect(self.selected)
    }

    /// How far the strip center may pan (in chip units) so the row's
    /// ends never over-shoot the window: when every chip fits, the row
    /// pins centered; otherwise scrolling spans first-to-last chip.
    fn chapter_pos_bounds(&self, n: usize) -> (f32, f32) {
        let cw = self.chapter_chip_w();
        let step = (cw + self.tuning.strip_gap).max(1.0);
        let edge = ((self.viewport.width - cw) * 0.5 - 24.0).max(0.0) / step;
        let last = n.saturating_sub(1) as f32;
        if last <= edge * 2.0 {
            let mid = last * 0.5;
            (mid, mid)
        } else {
            (edge, last - edge)
        }
    }

    /// Which chapter chip is under (x, y) — the filmstrip hit-test, at
    /// the bar's current slide position.
    fn chapter_chip_at(&self, x: f32, y: f32) -> Option<usize> {
        let bar = self.chapters.as_ref().filter(|b| b.open)?;
        let n = bar.times.as_ref()?.len();
        if n == 0 {
            return None;
        }
        let cw = self.chapter_chip_w();
        let (_, ch, base_y) = self.strip_geom();
        let sy = base_y + (1.0 - bar.slide) * (ch + 44.0);
        if y < sy - 8.0 || y > sy + ch + 8.0 {
            return None;
        }
        let step = cw + self.tuning.strip_gap;
        let rel = (x - self.viewport.width * 0.5) / step + bar.pos;
        let i = rel.round();
        if i < 0.0 || i as usize >= n {
            return None;
        }
        ((rel - i).abs() * step <= cw * 0.5).then_some(i as usize)
    }

    /// Click on a chapter chip: jump the playing stream to that
    /// timestamp — an exact in-place seek, flashing the position bar
    /// like a `[`/`]` skip.
    fn chapter_seek(&mut self, i: usize) {
        let Some(b) = &self.chapters else { return };
        let Some(&t) = b.times.as_ref().and_then(|ts| ts.get(i)) else {
            return;
        };
        let Some(l) = &self.live_sel else { return };
        if l.path != b.path {
            return;
        }
        // Stay a frame short of the end, like scrub_seek: an exact seek
        // to the tail lands on EOF and loops back to 0:00.
        let target = match l.duration.filter(|d| *d > 0.05) {
            Some(d) => t.clamp(0.0, (d - 0.05).max(0.0)),
            None => t.max(0.0),
        };
        l.player.seek(target, true);
        self.skip_flash_at = Some(Instant::now());
        self.wake(1.5);
    }

    /// The chapter the stream is currently inside (index of the last
    /// start ≤ position), for the bar's highlight.
    fn current_chapter(&self, bar: &ChapterBar) -> Option<usize> {
        let times = bar.times.as_ref().filter(|ts| !ts.is_empty())?;
        let l = self.live_sel.as_ref().filter(|l| l.path == bar.path)?;
        let p = match bar.duration.or(l.duration).filter(|d| *d > 0.05) {
            Some(d) => l.position().rem_euclid(d),
            None => l.position(),
        };
        Some(times.partition_point(|&t0| t0 <= p).saturating_sub(1))
    }

    /// True while animation/live playback should rest because the window
    /// lost focus (default on; `p` toggles, `pause_unfocused` configures).
    fn paused(&self) -> bool {
        !self.focused && self.tuning.pause_unfocused && self.focus_pause_on
    }

    /// May background prewarms (fullview's chapter probe, the on-demand
    /// anim sheet) start right now? Only once the video the user is
    /// watching is actually on screen — its cold spawn owns the CPU and
    /// the media engine until then (the same principle that staggers
    /// warm-pool spawns). Levels without live video don't wait, and a
    /// stream that never delivers releases the gate after a short grace.
    fn prewarm_ok(&self) -> bool {
        if !self.level().live() || self.paused() || self.demo {
            return true;
        }
        let ready = self.live_sel.as_ref().is_some_and(|l| {
            l.first_frame.is_some()
                && self
                    .clips
                    .get(self.selected)
                    .is_some_and(|c| c.path == l.path)
        });
        ready || self.sel_changed_at.elapsed().as_secs_f32() > PREWARM_GRACE_S
    }

    /// A warm-pool decoder is mid cold-spawn (no frames buffered yet).
    /// Sheet generation — nine ffmpeg decodes — additionally waits for
    /// this to clear, so the worst case never stacks three decode chains
    /// (selected stream + filling warm lane + storyboard) at once. The
    /// 5s dead-decoder escape in update_live bounds the wait.
    fn warm_filling(&self) -> bool {
        self.warm
            .iter()
            .any(|w| w.player.buffered() == 0 && w.spawned.elapsed().as_secs_f32() < 5.0)
    }

    /// Effective animation level: CLI `--animation` beats the config.
    fn level(&self) -> AnimLevel {
        self.cli_animation.unwrap_or(self.tuning.animation)
    }

    /// Sprite sheets cycle (and generate) only at level `full`, further
    /// gated by the runtime `a` toggle.
    fn sheets_on(&self) -> bool {
        self.level().sheets() && self.anim_on
    }

    /// Keep the render loop awake for at least `secs` (covers fades and
    /// settle timers that plain spring-residual checks can't see).
    fn wake(&mut self, secs: f32) {
        self.wake_until = self
            .wake_until
            .max(Instant::now() + Duration::from_secs_f32(secs));
    }

    fn set_zoom(&mut self, target: f32) {
        let t = &self.tuning;
        self.zoom_target = target.clamp(t.zoom_min, t.zoom_max);
    }

    // --- per-frame ---

    fn drain_ingest(&mut self) {
        let Some(rx) = &self.rx else { return };
        // The D swap's clip found its new index this drain (only field
        // updates are legal while `rx` borrows self; the selection move
        // happens after the loop).
        let mut reselect: Option<usize> = None;
        let first_new = self.clips.len();
        loop {
            // Per-frame budget (P0.3): leave the rest for the next frame
            // — which the wake guarantees — instead of stalling this one.
            if self.clips.len() - first_new >= INGEST_DRAIN_BUDGET {
                self.waker.wake();
                break;
            }
            match rx.try_recv() {
                Ok(item) => {
                    if self.pending_reselect.as_deref() == Some(item.path.as_path()) {
                        self.pending_reselect = None;
                        reselect = Some(self.clips.len());
                    }
                    self.index.insert(item.path.clone(), self.clips.len());
                    // Cloud placeholders never request a thumbnail: reading
                    // the file would force iCloud to download it.
                    let thumb = if item.cloud {
                        Thumb::Failed
                    } else {
                        Thumb::None
                    };
                    // Background gen sweep: every discovered file gets its
                    // thumbnail generated (disk cache only) — behind any
                    // visible-thumb request, ahead of all anim sheets.
                    if item.readable && !item.cloud {
                        self.media.request_gen(item.path.clone());
                        self.jobs_total += 1;
                    }
                    self.clips.push(Clip {
                        path: item.path,
                        readable: item.readable,
                        cloud: item.cloud,
                        // Unknown until its gen-sweep job reports back (a
                        // cache hit completes near-instantly).
                        cached: false,
                        spawned: Instant::now(),
                        scale: 1.0,
                        emph: 0.0,
                        thumb,
                        anim: if item.cloud {
                            Thumb::Failed
                        } else {
                            Thumb::None
                        },
                    });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    log::info!("stdin closed — {} clips ingested", self.clips.len());
                    self.rx = None;
                    // The awaited clip never showed up (deleted/moved):
                    // stop shielding its stream; update_live reaps it.
                    self.pending_reselect = None;
                    break;
                }
            }
        }
        // Spawn fade-in only needs the loop hot if a newcomer is actually
        // on screen — a bulk stream appending offscreen rows must not keep
        // the GPU presenting (P0.2). Offscreen arrivals still repaint once
        // (their send fired the waker) so the title count stays fresh.
        if first_new < self.clips.len() {
            let lay = self.layout();
            let (_, last_vis) = self.visible_rows(&lay, PREFETCH_ROWS);
            if first_new / lay.cols.max(1) <= last_vis {
                self.wake(0.6);
            }
        }
        if let Some(i) = reselect {
            self.selected = i;
            self.sel_changed_at = Instant::now();
            if let Some(l) = &mut self.live_sel
                && l.path == self.clips[i].path
            {
                l.clip = i; // the surviving stream, renumbered
            }
            self.scroll_to_selected();
            if self.quickview {
                self.strip_pos = i as f32; // snap; don't slide across the dir
                self.strip_target = self.strip_pos;
            }
        }
    }

    fn step(&mut self, dt: f32) {
        let t = self.tuning.clone();
        // Animation level "none": every chase/spring covers its full
        // distance in one frame — the UI snaps instead of tweening.
        let ui = self.level().ui();
        let a_of = |k: f32| if ui { alpha(k, dt) } else { 1.0 };

        // Zoom spring, anchored so the content at the viewport center stays
        // put while tile size (and column count) reflows around it.
        let old_zoom = self.zoom;
        self.zoom += (self.zoom_target - self.zoom) * a_of(t.zoom_smoothing);
        if (self.zoom - old_zoom).abs() > 1e-5 {
            let old_h = self.content_height(&self.layout_with(old_zoom));
            let new_h = self.content_height(&self.layout_with(self.zoom));
            if old_h > 0.0 {
                let half = self.viewport.height * 0.5;
                self.scroll = (self.scroll + half) / old_h * new_h - half;
                self.scroll_target = (self.scroll_target + half) / old_h * new_h - half;
            }
        }

        let lay = self.layout();

        // Optional extra inertia (off by default; macOS supplies momentum).
        if t.pan_inertia > 0.0 && self.last_scroll_event.elapsed().as_secs_f32() > 0.04 {
            self.scroll_target += self.scroll_vel * dt;
            self.scroll_vel *= t.pan_inertia.powf(dt * 60.0);
        }

        // Rubber-band the target back into bounds.
        let max = self.max_scroll(&lay);
        if self.scroll_target < 0.0 {
            self.scroll_target *= 1.0 - alpha(t.rubber_band, dt);
            if self.scroll_target > -0.5 {
                self.scroll_target = 0.0;
            }
            self.scroll_vel = 0.0;
        } else if self.scroll_target > max {
            self.scroll_target =
                max + (self.scroll_target - max) * (1.0 - alpha(t.rubber_band, dt));
            if self.scroll_target - max < 0.5 {
                self.scroll_target = max;
            }
            self.scroll_vel = 0.0;
        }

        // Camera chases its target (key moves use a gentler chase).
        self.scroll += (self.scroll_target - self.scroll) * a_of(self.chase);

        // No grid hover under ANY modal (quickview frost, fullview's
        // black, the chapter bar): the pointer still sits "over" a
        // hidden/frozen tile — without this gate every pointer rest
        // cold-spawned a hover decoder for a clip nobody can see,
        // stealing CPU and the media engine from the video being watched.
        let modal = self.quickview || self.fullview;
        let hover_now = if modal || self.chapters.is_some() {
            None
        } else {
            self.tile_at(&lay, self.cursor.0, self.cursor.1)
        };
        if hover_now != self.hovered {
            self.hovered = hover_now;
            self.hover_changed_at = Instant::now();
        }
        // Same for the filmstrip when fullview covers quickview: its
        // chips are behind the opaque backdrop, so no hover lane there.
        let strip_hover_now = if self.quickview && !self.fullview && self.chapters.is_none() {
            self.strip_chip_at(self.cursor.0, self.cursor.1)
        } else {
            None
        };
        if strip_hover_now != self.strip_hover {
            self.strip_hover = strip_hover_now;
            self.hover_changed_at = Instant::now();
        }

        // Selection stands out more the further you zoom out.
        let boost = (1.0 / self.zoom.max(0.05))
            .powf(t.selection_zoom_boost)
            .clamp(1.0, 2.2);
        let sel_scale = t.selection_scale * boost;

        // Tile scale + emphasis springs (selected > hover > rest); both
        // keyboard moves and hover ride the same tween. Track whether any
        // spring is still in flight for idle throttling. Behind a modal
        // NONE of this runs: the backdrop is a frozen snapshot, so grid
        // springs and camera glides would be invisible work that only
        // kept the loop hot — the camera snaps to its target instead
        // (the state stays right for the eventual exit) and the per-clip
        // spring sweep over the whole library is skipped outright.
        let mut motion = !modal
            && ((self.scroll_target - self.scroll).abs() > 0.3
                || (self.zoom_target - self.zoom).abs() > 1e-3);
        if modal {
            self.scroll = self.scroll_target;
        }
        let a = a_of(t.scale_smoothing);
        if !modal {
            for (i, clip) in self.clips.iter_mut().enumerate() {
                let emphasized = i == self.selected || Some(i) == self.hovered;
                let target = if i == self.selected {
                    sel_scale
                } else if Some(i) == self.hovered {
                    t.hover_scale
                } else {
                    1.0
                };
                let e_target = emphasized as u8 as f32;
                if (target - clip.scale).abs() > 0.002 || (e_target - clip.emph).abs() > 0.005 {
                    motion = true;
                }
                clip.scale += (target - clip.scale) * a;
                clip.emph += (e_target - clip.emph) * a;
            }
        }
        self.motion = motion;

        // Quickview filmstrip slides with the same curve as keyboard moves.
        // While a scroll/scrub gesture is live the strip eases toward the
        // free-float `strip_target`; once the gesture settles the target homes
        // on the selected chip, so wheel input flows and then snaps magnetic.
        if self.quickview {
            if self.last_scroll_event.elapsed().as_secs_f32() > 0.12 {
                self.strip_target = self.selected as f32;
            }
            self.strip_pos += (self.strip_target - self.strip_pos) * a_of(t.strip_snap_strength);
            if (self.strip_target - self.strip_pos).abs() > 0.001 {
                self.motion = true;
            }
        }

        // Chapter bar: the slide-in rides the same spring family, and
        // the strip pans toward its scroll target like the filmstrip.
        // Once a closing bar's slide lands, the state drops.
        if let Some(b) = &mut self.chapters {
            let slide_target = if b.open { 1.0 } else { 0.0 };
            b.slide += (slide_target - b.slide) * a_of(t.strip_snap_strength);
            b.pos += (b.target - b.pos) * a_of(t.strip_snap_strength);
            if (slide_target - b.slide).abs() > 0.002 || (b.target - b.pos).abs() > 0.001 {
                self.motion = true;
            }
        }
        if self
            .chapters
            .as_ref()
            .is_some_and(|b| !b.open && b.slide < 0.005)
        {
            self.chapters = None;
        }
    }

    /// Filmstrip geometry: NOMINAL (16:9) chip width, fixed chip height,
    /// and the strip's top y. Real chip widths are per-clip true aspect
    /// (`chip_aspect`); the nominal width paces scroll sensitivity and
    /// pos-bounds math.
    fn strip_geom(&self) -> (f32, f32, f32) {
        let ch = self.tuning.strip_height.max(24.0);
        let cw = ch * 16.0 / 9.0;
        (cw, ch, self.viewport.height - ch - 22.0)
    }

    /// Displayed aspect (w/h) of clip `i`'s strip chip: the thumbnail's
    /// true aspect (fit-scaled, rotation already applied), clamped so a
    /// pathological source stays browsable; 16:9 until the thumb lands.
    /// Chips keep the strip's fixed HEIGHT and vary in width — portrait
    /// clips are narrower, never taller.
    fn chip_aspect(&self, i: usize) -> f32 {
        let a = match self.clips.get(i).map(|c| &c.thumb) {
            Some(&Thumb::Ready { tw, th, .. }) => tw as f32 / (th as f32).max(1.0),
            _ => 16.0 / 9.0,
        };
        a.clamp(9.0 / 16.0, 2.4)
    }

    /// Visible filmstrip chips around fractional position `pos`:
    /// `(clip index, center x, width)`. Widths are per-clip true aspect
    /// at the strip's fixed height, so spacing is cumulative: chip
    /// floor(pos) anchors at the window center (shifted by the
    /// fractional part of one center-to-center step) and the walk
    /// extends both ways until chips leave the screen.
    fn strip_layout(&self, pos: f32) -> Vec<(usize, f32, f32)> {
        let n = self.clips.len();
        if n == 0 {
            return Vec::new();
        }
        let (_, ch, _) = self.strip_geom();
        let gap = self.tuning.strip_gap;
        let vw = self.viewport.width;
        let w_of = |i: usize| ch * self.chip_aspect(i);
        let base = (pos.max(0.0) as usize).min(n - 1);
        let frac = (pos - base as f32).clamp(0.0, 1.0);
        let shift = if base + 1 < n {
            frac * ((w_of(base) + w_of(base + 1)) * 0.5 + gap)
        } else {
            0.0
        };
        let cx0 = vw * 0.5 - shift;
        let mut out = vec![(base, cx0, w_of(base))];
        // Margin so chips scaled up by selection/hover still enter the
        // walk before their unscaled box would.
        let m = ch * 1.5;
        let (mut cx, mut i) = (cx0, base);
        while i + 1 < n {
            cx += (w_of(i) + w_of(i + 1)) * 0.5 + gap;
            i += 1;
            if cx - w_of(i) * 0.5 > vw + m {
                break;
            }
            out.push((i, cx, w_of(i)));
        }
        let (mut cx, mut i) = (cx0, base);
        while i > 0 {
            cx -= (w_of(i) + w_of(i - 1)) * 0.5 + gap;
            i -= 1;
            if cx + w_of(i) * 0.5 < -m {
                break;
            }
            out.push((i, cx, w_of(i)));
        }
        out
    }

    /// Which filmstrip chip is under (x, y), if any.
    fn strip_chip_at(&self, x: f32, y: f32) -> Option<usize> {
        if !self.quickview || self.clips.is_empty() {
            return None;
        }
        let (_, ch, sy) = self.strip_geom();
        if y < sy - 8.0 || y > sy + ch + 8.0 {
            return None;
        }
        self.strip_layout(self.strip_pos)
            .into_iter()
            .find(|&(_, cx, w)| (x - cx).abs() <= w * 0.5)
            .map(|(i, _, _)| i)
    }

    /// Rows currently on screen, extended by `margin` prefetch rows.
    fn visible_rows(&self, lay: &Layout, margin: usize) -> (usize, usize) {
        let g = self.tuning.gap;
        let first = (((self.scroll - g) / lay.cell_h).floor().max(0.0)) as usize;
        let last = (((self.scroll + self.viewport.height) / lay.cell_h).ceil()) as usize;
        (first.saturating_sub(margin), last + margin)
    }

    /// Zone rows ordered center-outward, for prioritized requests.
    fn zone_rows(&self, lay: &Layout) -> Vec<usize> {
        let (first_row, last_row) = self.visible_rows(lay, PREFETCH_ROWS);
        let center = ((self.scroll + self.viewport.height * 0.5) / lay.cell_h).max(0.0) as i64;
        let mut rows: Vec<usize> = (first_row..=last_row).collect();
        rows.sort_by_key(|r| (*r as i64 - center).abs());
        rows
    }

    /// Queue thumbnail generation for visible + nearby tiles, center-out,
    /// within the atlas slot budget: statics claim budget first, anim
    /// sheets get what's left. Without the budget, a big viewport demands
    /// more slots than exist and eviction churns everything forever.
    fn request_visible_thumbs(&mut self, lay: &Layout) {
        if self.demo {
            return;
        }
        let rows = self.zone_rows(lay);
        let mut budget = self.atlas_cfg.slots() as i64 - 8; // headroom incl. live slot

        'statics: for &row in &rows {
            for col in 0..lay.cols {
                let i = row * lay.cols + col;
                let Some(clip) = self.clips.get_mut(i) else {
                    break;
                };
                if !clip.readable || matches!(clip.thumb, Thumb::Failed) {
                    continue;
                }
                if budget <= 0 {
                    break 'statics;
                }
                budget -= 1;
                if matches!(clip.thumb, Thumb::None) {
                    self.media.request(clip.path.clone());
                    clip.thumb = Thumb::Pending;
                    self.jobs_total += 1;
                }
            }
        }

        if !self.sheets_on() || lay.tile_w < self.tuning.anim_min_tile_w {
            return;
        }
        'anims: for &row in &rows {
            for col in 0..lay.cols {
                let i = row * lay.cols + col;
                let Some(clip) = self.clips.get_mut(i) else {
                    break;
                };
                if !clip.readable || matches!(clip.anim, Thumb::Failed) {
                    continue;
                }
                if budget <= 0 {
                    break 'anims;
                }
                match clip.anim {
                    Thumb::Ready { .. } | Thumb::Pending => budget -= 1,
                    Thumb::None if matches!(clip.thumb, Thumb::Ready { .. }) => {
                        budget -= 1;
                        self.media.request_anim(clip.path.clone());
                        clip.anim = Thumb::Pending;
                        self.jobs_total += 1;
                    }
                    _ => {}
                }
            }
        }
    }

    /// The seekbar's storyboard preview samples the anim sheet, which the
    /// grid only generates at animation level `full` — so quickview
    /// requests the selected clip's sheet on demand (g² cheap seeked
    /// extracts, disk-cached) at any level. One clip at a time, never
    /// library-wide (PLAN.md §14 M8). `request_anim_now`, not
    /// `request_anim`: the bulk-sheet tier sits below the library gen
    /// sweep, which can back up for hours after a recipe change — the
    /// user hovering the seekbar can't wait behind that.
    fn request_quickview_sheet(&mut self) {
        // Fullview wants the sheet too — the chapter bar's chips sample
        // it, and requesting on fullview ENTRY (not bar-open) means the
        // storyboard is generating/cached before g is ever pressed. But
        // never before the video being watched has its first frame
        // (prewarm_ok): sheet generation is nine niced ffmpeg decodes,
        // and racing them against the interactive cold spawn is exactly
        // the jank the priority tiers exist to prevent.
        let want = ((self.quickview && self.tuning.seekbar_thumb_width >= 8.0) || self.fullview)
            && self.prewarm_ok()
            && !self.warm_filling();
        if !want {
            return;
        }
        let Some(clip) = self.clips.get_mut(self.selected) else {
            return;
        };
        if clip.readable
            && !clip.cloud
            && matches!(clip.anim, Thumb::None)
            && matches!(clip.thumb, Thumb::Ready { .. })
        {
            self.media.request_anim_now(clip.path.clone());
            clip.anim = Thumb::Pending;
            self.jobs_total += 1;
        }
    }

    /// Collect finished thumbnails into atlas uploads for this frame.
    fn drain_media(&mut self, lay: &Layout) -> Vec<ThumbUpload> {
        let mut uploads = Vec::new();
        let was_pending = self.jobs_total > self.jobs_done;
        // While the selected stream is actively presenting, background
        // thumb bursts trickle in under a much smaller per-frame budget:
        // the full 64-upload batch staged ~80MB of texture writes
        // between two video frames — a visible playback hitch.
        let budget = if self
            .live_sel
            .as_ref()
            .is_some_and(|l| l.first_frame.is_some())
            && !self.sel_parked
            && !self.paused()
        {
            MEDIA_UPLOAD_BUDGET_LIVE
        } else {
            MEDIA_UPLOAD_BUDGET
        };
        while uploads.len() < budget {
            let Some(result) = self.media.try_recv() else {
                break;
            };
            // The chapter probe's answer lives outside the jobs ledger
            // (it isn't cached background work). Cache it per clip —
            // fullview pre-warms probes ahead of the bar opening — and
            // install it into the bar when one is waiting.
            let result = match result {
                ThumbResult::ChapterTimes {
                    path,
                    times,
                    duration,
                } => {
                    self.chapter_probe
                        .insert(path.clone(), Some((times.clone(), duration)));
                    self.apply_chapter_plan(&path, &times, duration);
                    continue;
                }
                other => other,
            };
            self.jobs_done += 1;
            // Visual results (a tile crossfades in) keep the loop hot for
            // the fade; GenDone is disk-cache-only — the waker-driven
            // redraw that delivered it already repainted the progress
            // bar, and a multi-hour sweep must not animate continuously
            // between completions (P0.2).
            if !matches!(result, ThumbResult::GenDone { .. }) {
                self.wake(0.6); // thumb crossfade
            }
            match result {
                ThumbResult::Ready { path, w, h, rgba } => {
                    let Some(&i) = self.index.get(&path) else {
                        continue;
                    };
                    // Decoded pixels arrived, so the artifact is on disk —
                    // cached even if the atlas drops it below.
                    self.clips[i].cached = true;
                    // Idempotent install (P1.2 follow-up): coalescing can
                    // legitimately deliver the same artifact twice (a D
                    // swap re-requests while the first job is still in
                    // flight). Free the slot this clip already owns first
                    // — otherwise it lingers with a stale owner, and its
                    // eventual eviction clears the clip's CURRENT thumb.
                    if let Thumb::Ready { slot, .. } = self.clips[i].thumb {
                        self.slots[slot] = None;
                    }
                    let Some(slot) = self.alloc_slot(lay, SlotKind::Static) else {
                        // Atlas momentarily full: drop the pixels but stay
                        // retryable — the disk cache makes the redo cheap.
                        // (A Failed latch here permanently "lost" thumbs.)
                        log::debug!("static dropped, atlas full: {}", path.display());
                        self.clips[i].thumb = Thumb::None;
                        continue;
                    };
                    log::debug!("static ready: clip {i} -> slot {slot} ({w}x{h})");
                    self.slots[slot] = Some((i, SlotKind::Static));
                    self.clips[i].thumb = Thumb::Ready {
                        slot,
                        at: Instant::now(),
                        tw: w,
                        th: h,
                    };
                    uploads.push(ThumbUpload { slot, w, h, rgba });
                }
                ThumbResult::Failed { path } => {
                    log::debug!("thumbnail failed: {}", path.display());
                    if let Some(&i) = self.index.get(&path) {
                        self.clips[i].thumb = Thumb::Failed;
                    }
                }
                ThumbResult::AnimReady { path, w, h, rgba } => {
                    let Some(&i) = self.index.get(&path) else {
                        continue;
                    };
                    // Same idempotent install as the static arm above.
                    if let Thumb::Ready { slot, .. } = self.clips[i].anim {
                        self.slots[slot] = None;
                    }
                    let Some(slot) = self.alloc_slot(lay, SlotKind::Anim) else {
                        log::debug!("anim dropped, atlas full: {}", path.display());
                        self.clips[i].anim = Thumb::None;
                        continue;
                    };
                    log::debug!("anim ready: clip {i} -> slot {slot} ({w}x{h})");
                    self.slots[slot] = Some((i, SlotKind::Anim));
                    self.clips[i].anim = Thumb::Ready {
                        slot,
                        at: Instant::now(),
                        tw: w,
                        th: h,
                    };
                    uploads.push(ThumbUpload { slot, w, h, rgba });
                }
                ThumbResult::AnimFailed { path } => {
                    log::debug!("anim sheet failed: {}", path.display());
                    if let Some(&i) = self.index.get(&path) {
                        self.clips[i].anim = Thumb::Failed;
                    }
                }
                ThumbResult::GenDone { path } => {
                    // Counted once at the top of the loop like every other
                    // result — a second increment here overran jobs_total.
                    // The sweep just proved (or wrote) this clip's cache
                    // entry: it becomes fair game for jump/shuffle.
                    if let Some(&i) = self.index.get(&path) {
                        self.clips[i].cached = true;
                    }
                }
                // Consumed before the ledger above.
                ThumbResult::ChapterTimes { .. } => {}
            }
        }
        // Budget hit — more results are likely waiting; take them next
        // frame (P0.3).
        if uploads.len() >= budget {
            self.waker.wake();
        }
        // Batch just completed: the progress bar lingers, then fades over
        // 0.7s (build_frame) — keep the loop hot through it, once, on the
        // transition only.
        if was_pending && self.jobs_total > 0 && self.jobs_done >= self.jobs_total {
            self.wake(1.0);
        }
        uploads
    }

    /// First free atlas slot, or evict by class: out-of-zone anims first,
    /// then out-of-zone statics, then in-zone anims — never an in-zone
    /// static or the live slot. Returns None (caller drops the pixels) when
    /// nothing evictable remains; the request budget makes that rare.
    fn alloc_slot(&mut self, lay: &Layout, incoming: SlotKind) -> Option<usize> {
        let center_row = ((self.scroll + self.viewport.height * 0.5) / lay.cell_h).max(0.0) as i64;
        let (zone_first, zone_last) = self.visible_rows(lay, PREFETCH_ROWS);
        let mut best: Option<(usize, u8, i64)> = None;
        for (j, owner) in self.slots.iter().enumerate() {
            let Some((owner, kind)) = owner else {
                return Some(j);
            };
            let row = owner / lay.cols;
            let dist = (row as i64 - center_row).abs();
            let in_zone = row >= zone_first && row <= zone_last;
            let class: u8 = match (in_zone, kind) {
                (_, SlotKind::Live) => 0, // never evicted
                (true, SlotKind::Static) => 0,
                (true, SlotKind::Anim) => 1,
                (false, SlotKind::Static) => 2,
                (false, SlotKind::Anim) => 3,
            };
            if best.is_none_or(|(_, bc, bd)| (class, dist) > (bc, bd)) {
                best = Some((j, class, dist));
            }
        }
        let (j, class, _) = best.expect("atlas has at least one slot");
        let min_class = match incoming {
            // A static or the live frame may displace in-zone anims;
            // an anim sheet may not displace anything in-zone.
            SlotKind::Static | SlotKind::Live => 1,
            SlotKind::Anim => 2,
        };
        if class < min_class {
            return None;
        }
        if let Some((victim, kind)) = self.slots[j] {
            log::debug!("evict slot {j} (clip {victim})");
            match kind {
                SlotKind::Static => self.clips[victim].thumb = Thumb::None,
                SlotKind::Anim => self.clips[victim].anim = Thumb::None,
                SlotKind::Live => {}
            }
        }
        self.slots[j] = None;
        Some(j)
    }

    /// Live in-tile playback for the selected and hovered tiles: each lane
    /// starts once its target settles, stops the moment it moves, and
    /// pumps the newest decoded frame into a never-evicted atlas slot.
    fn update_live(&mut self, lay: &Layout, uploads: &mut Vec<ThumbUpload>) {
        // Live video exists at animation level `normal` and up: the
        // selected stream (tile + quickview modal) and the hover lane.
        let live_on = !self.demo && !self.paused() && self.level().live();
        let delay_ms = self.tuning.live_delay_ms;
        let sel_target = live_on.then_some(self.selected);
        // The hover lane: in the grid, the hovered tile; in quickview,
        // the hovered filmstrip chip. Never the selected clip (its
        // stream owns that).
        let hover_target = if !live_on {
            None
        } else if self.quickview {
            self.strip_hover.filter(|h| *h != self.selected)
        } else {
            self.hovered.filter(|h| *h != self.selected)
        };

        // A D swap in flight: clip indices are churning, so don't warm,
        // spawn or reap by index — just keep the shielded stream playing
        // until its path streams back in and the selection re-lands.
        let pending = self.pending_reselect.is_some();

        // Reap failed lanes (P0.4): an unopenable/undecodable stream never
        // produces frames — left installed it would keep `animating` true
        // forever and hold its tile hostage. Drop it (the static thumb
        // takes over) and cool the path down so the settle logic doesn't
        // respawn a doomed decoder every frame.
        if self.live_sel.as_ref().is_some_and(|l| l.player.failed()) {
            let l = self.live_sel.take().unwrap();
            log::warn!(
                "live stream failed — falling back to thumbnail: {}",
                l.path.display()
            );
            self.live_retry.insert(l.path, Instant::now());
        }
        if self.live_hover.as_ref().is_some_and(|l| l.player.failed()) {
            let l = self.live_hover.take().unwrap();
            self.slots[l.slot] = None;
            if let Some(c) = self.clips.get(l.clip) {
                log::warn!("hover stream failed: {}", c.path.display());
                self.live_retry.insert(c.path.clone(), Instant::now());
            }
        }
        let mut i = 0;
        while i < self.warm.len() {
            if self.warm[i].player.failed() {
                let w = self.warm.remove(i);
                log::debug!("warm decoder failed: {}", w.path.display());
                self.live_retry.insert(w.path, Instant::now());
            } else {
                i += 1;
            }
        }

        // Pre-warm decoders for the four movement destinations (±1 and
        // ±row) so a selection move shows video instantly instead of
        // paying a cold spawn (open + decode to the thumb frame — no
        // longer the CLI's ~1s floor, but still the visible latency).
        let mut warm_targets: Vec<usize> = Vec::new();
        if live_on && !pending {
            let s = self.selected;
            let cols = lay.cols.max(1);
            let n = self.clips.len();
            let mut push = |i: usize| {
                if i < n && i != s && !warm_targets.contains(&i) {
                    warm_targets.push(i);
                }
            };
            // Warm-ups run one at a time, so this order is a priority.
            // Browsing overwhelmingly flows right (often repeatedly),
            // then down; 'up' is rare enough to stay cold — its slot
            // goes to the SECOND clip to the right instead, so a double
            // right-tap is instant too.
            push(s + 1);
            push(s + 2);
            push(s + cols);
            if s > 0 {
                push(s - 1);
            }
        }

        // Stop lanes whose target moved away. In quickview the selected
        // stream demotes to a warm neighbor instead of dying — reversing
        // direction picks it right back up, still on its timeline.
        if self.live_sel.as_ref().is_some_and(|l| {
            sel_target != Some(l.clip) && self.pending_reselect.as_deref() != Some(l.path.as_path())
        }) {
            let l = self.live_sel.take().unwrap();
            if warm_targets.contains(&l.clip) {
                self.warm.push(l);
            }
        }
        if let Some(l) = &self.live_hover
            && hover_target != Some(l.clip)
        {
            let slot = l.slot;
            self.live_hover = None;
            self.slots[slot] = None;
        }

        // Start lanes whose target has settled. Quickview skips the settle
        // delay — the user's explicit action goes to the forefront — and
        // promotes a pre-warmed neighbor when one is ready: its queue
        // already holds due frames, so the video shows this same tick.
        // Promotion MUST happen before the warm pool is pruned: the pool
        // is keyed by the new selection's neighbors, which never include
        // the selection itself — pruning first would kill the very decoder
        // that was warmed for this moment (the bug that made every advance
        // pay full cold-spawn latency).
        if self.live_sel.is_none()
            && !pending
            && let Some(i) = sel_target
        {
            if let Some(pos) = self.warm.iter().position(|w| w.clip == i) {
                let l = self.warm.remove(pos);
                log::debug!(
                    "promoted warm decoder for clip {i} ({} frames buffered)",
                    l.player.buffered()
                );
                self.live_sel = Some(l);
            } else if self.quickview || self.sel_changed_at.elapsed().as_millis() as f32 >= delay_ms
            {
                self.live_sel = self.start_sel_live(i);
            }
        }
        self.warm.retain(|w| warm_targets.contains(&w.clip));
        // New warm decoders spawn only once the selection has settled AND
        // the selected stream has its first frame on screen — the clip
        // being watched owns the CPU until then (user attention first).
        // Promotion above never waits; this only staggers fresh spawns.
        let sel_ready = self
            .live_sel
            .as_ref()
            .is_some_and(|l| l.first_frame.is_some());
        // ...and one at a time: a cold spawn burns a core until its first
        // GOP lands (in-process now, but decode-to-target still costs), so
        // the next warm-up starts only after the previous one has produced
        // a frame and stalled. The playing video's decoder is never
        // outnumbered. (5s escape hatch: a dead decoder never buffers and
        // must not block warming the other destinations forever.)
        let warming_up = self
            .warm
            .iter()
            .any(|w| w.player.buffered() == 0 && w.spawned.elapsed().as_secs_f32() < 5.0);
        if sel_ready
            && !warming_up
            && self.sel_changed_at.elapsed().as_millis() as f32 >= delay_ms
            && let Some(&i) = warm_targets
                .iter()
                .find(|&&i| self.warm.iter().all(|w| w.clip != i))
            && let Some(l) = self.start_sel_live(i)
        {
            self.warm.push(l);
        }
        if self.live_hover.is_none()
            && let Some(i) = hover_target
        {
            // Filmstrip chips skip the settle delay: hovering one in
            // quickview is a deliberate pointer act, like the modal
            // itself (grid hover keeps the delay — the cursor crosses
            // tiles it doesn't mean).
            if self.quickview || self.hover_changed_at.elapsed().as_millis() as f32 >= delay_ms {
                self.live_hover = self.start_live(lay, i);
            }
        }

        if let Some(live) = &mut self.live_hover
            && let Some(rgba) = live.player.take_frame()
        {
            if live.first_frame.is_none() {
                live.first_frame = Some(Instant::now());
            }
            uploads.push(ThumbUpload {
                slot: live.slot,
                w: live.player.w,
                h: live.player.h,
                rgba,
            });
        }
        // Park the selected stream while its tile is offscreen and no
        // modal shows it (P0.4): stop draining and the bounded queue
        // stalls the decoder — decode, copy, upload and mip generation
        // all cease — while the stream object and its timeline survive.
        // Panning back resumes the same decoder; no respawn. (Keyboard
        // moves always scroll to the selection, so only trackpad panning
        // gets here.)
        self.sel_parked = self.live_sel.is_some() && !self.quickview && !self.fullview && {
            let (first, last) = self.visible_rows(lay, 0);
            let row = self.selected / lay.cols.max(1);
            !(first..=last).contains(&row)
        };
        if !self.sel_parked
            && let Some(live) = &mut self.live_sel
            && let Some(rgba) = live.player.take_frame()
        {
            if live.first_frame.is_none() {
                live.first_frame = Some(Instant::now());
                log::debug!(
                    "sel live clip {} first frame {:.0}ms after spawn",
                    live.clip,
                    live.spawned.elapsed().as_secs_f32() * 1000.0
                );
            }
            if self.hires_shown.as_ref() != Some(&live.path) {
                self.hires_shown = Some(live.path.clone());
            }
            self.hires_frame = Some(HiresFrame {
                w: live.player.w,
                h: live.player.h,
                rgba,
            });
        }
    }

    /// A live decoder for `path` failed recently — hold off respawning
    /// (`LIVE_RETRY_COOLDOWN_S`); transient failures recover after it.
    fn live_cooling(&self, path: &std::path::Path) -> bool {
        self.live_retry
            .get(path)
            .is_some_and(|at| at.elapsed().as_secs_f32() < LIVE_RETRY_COOLDOWN_S)
    }

    /// Cache entries written before `Meta.pix_fmt` existed force the
    /// software scale chain (the hw gate can't run blind); queue a
    /// one-time background reprobe so the clip's NEXT spawn goes
    /// hardware. Never probe on the render thread — that's a hitch.
    fn heal_meta(&mut self, meta: Option<&sb_media::Meta>, path: &std::path::Path) {
        if meta.is_some_and(|m| m.pix_fmt.is_none()) && self.reprobed.insert(path.to_path_buf()) {
            self.media.request_reprobe(path.to_path_buf());
        }
    }

    /// The selected clip's decoder: natural resolution, capped at the hires
    /// texture (never upscaled past the source when its dims are known).
    /// Resident and seekable — repositioning after this is `player.seek()`,
    /// never another spawn.
    fn start_sel_live(&mut self, i: usize) -> Option<SelLive> {
        let clip = self.clips.get(i)?;
        let (tw, th, path) = match clip.thumb {
            Thumb::Ready { tw, th, .. } if clip.readable && !clip.cloud => {
                (tw, th, clip.path.clone())
            }
            _ => return None,
        };
        if self.live_cooling(&path) {
            return None;
        }
        let meta = sb_media::cached_meta(&path);
        let (mut sw, mut sh) = meta
            .as_ref()
            .and_then(|m| Some((m.width? as f32, m.height? as f32)))
            .unwrap_or((tw as f32 * 4.0, th as f32 * 4.0));
        // Phone footage stores portrait as landscape + a rotation tag;
        // the decoder auto-rotates, so a 90°/270° clip comes out with
        // width/height swapped versus the probed dims. Match that or
        // the scale filter stretches portrait back to landscape.
        if meta
            .as_ref()
            .and_then(|m| m.rotation)
            .is_some_and(|r| ((r / 90.0).round() as i64) % 2 != 0)
        {
            std::mem::swap(&mut sw, &mut sh);
        }
        let (bw, bh) = (self.atlas_cfg.hires_w as f32, self.atlas_cfg.hires_h as f32);
        let scale = (bw / sw).min(bh / sh).min(1.0);
        let (dw, dh) = (((sw * scale) as u32).max(2), ((sh * scale) as u32).max(2));
        // Start where the thumbnail was taken, so video continues from the
        // frame the tile already shows instead of jolting to 0:00.
        let duration = meta.as_ref().and_then(|m| m.duration);
        let seek = duration
            .map(|d| (d * sb_media::SEEK_FRACTION).max(0.0))
            .unwrap_or(0.0);
        self.heal_meta(meta.as_ref(), &path);
        let player = sb_media::SeekablePlayer::spawn(&path, dw, dh, seek, meta.as_ref())?;
        // Deadline-paced redraws (P1.4): a frame landing in a dry queue
        // must nudge the sleeping loop.
        player.set_notify(self.notify.clone());
        log::debug!("selected live {dw}x{dh} @{seek:.1}s: {}", path.display());
        Some(SelLive {
            clip: i,
            path,
            player,
            spawned: Instant::now(),
            first_frame: None,
            duration,
        })
    }

    fn start_live(&mut self, lay: &Layout, i: usize) -> Option<LiveState> {
        let clip = self.clips.get(i)?;
        // Decode at the static thumb's exact fit dimensions, so the
        // emphasized tile's aspect math applies unchanged.
        let (tw, th, path) = match clip.thumb {
            Thumb::Ready { tw, th, .. } if clip.readable && !clip.cloud => {
                (tw, th, clip.path.clone())
            }
            _ => return None,
        };
        if self.live_cooling(&path) {
            return None;
        }
        let slot = self.alloc_slot(lay, SlotKind::Live)?;
        // Start where the thumbnail was taken, so video continues from
        // the frame the tile already shows instead of jolting to 0:00.
        let meta = sb_media::cached_meta(&path);
        let seek = meta
            .as_ref()
            .and_then(|m| m.duration)
            .map(|d| (d * sb_media::SEEK_FRACTION).max(0.0))
            .unwrap_or(0.0);
        self.heal_meta(meta.as_ref(), &path);
        let Some(player) = sb_media::SeekablePlayer::spawn(&path, tw, th, seek, meta.as_ref())
        else {
            log::debug!("live preview failed to start: {}", path.display());
            return None;
        };
        player.set_notify(self.notify.clone()); // P1.4 dry-queue wake
        log::debug!("hover live: {}", path.display());
        self.slots[slot] = Some((i, SlotKind::Live));
        Some(LiveState {
            clip: i,
            player,
            slot,
            first_frame: None,
        })
    }

    fn update_title(&mut self) {
        let name = self
            .clips
            .get(self.selected)
            .and_then(|c| c.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Window-title label: the debug-quality label allowed pre-M7 (PLAN.md §9).
        let title = format!("switchblade — {} clips — {name}", self.clips.len());
        if title != self.title {
            self.title = title.clone();
            self.cmds.push(WindowCommand::SetTitle(title));
        }
    }

    fn build_frame(&mut self) -> Frame {
        let t = self.tuning.clone();
        let lay = self.layout();
        let (first_row, last_row) = self.visible_rows(&lay, 0);
        // Animation level "none": fades/crossfades complete instantly.
        let ui = self.level().ui();

        let now = Instant::now();
        let anim_t = now.saturating_duration_since(self.t0).as_secs_f32();
        let skip_bar = self.skip_bar();
        self.anim_rendered = false;
        let mut tiles = Vec::new();
        // Z-order: grid tiles, then the hovered tile lifted above its
        // neighbors, then the selected tile on top of everything.
        let mut hovered_group: Vec<Tile> = Vec::new();
        let mut selected_group: Vec<Tile> = Vec::new();

        // Modal views freeze the world behind them: the backdrop is a
        // STATIC snapshot of the grid from the moment the modal opened
        // (quickview blurs it; fullview covers it entirely, so it draws
        // NOTHING), and none of the per-tile work below — dots, sheet
        // frames, skip bars — runs while one is up. One of the bigger
        // playback levers: cycling sheets behind the frost kept the loop
        // at display rate for as long as quickview stayed open. A resize
        // invalidates the snapshot (it stores window positions).
        let modal = self.quickview || self.fullview;
        if !modal
            || self
                .frozen_grid
                .as_ref()
                .is_some_and(|(w, h, _)| *w != self.viewport.width || *h != self.viewport.height)
        {
            self.frozen_grid = None;
        }
        if let Some((_, _, frozen)) = &self.frozen_grid {
            tiles = frozen.clone();
        } else if !self.fullview {
            for row in first_row..=last_row {
                for col in 0..lay.cols {
                    let i = row * lay.cols + col;
                    if i >= self.clips.len() {
                        break;
                    }
                    let clip = &self.clips[i];

                    // Spawn fade/scale-in.
                    let fade = if !ui {
                        1.0
                    } else {
                        match now.checked_duration_since(clip.spawned) {
                            Some(el) => {
                                (el.as_secs_f32() * 1000.0 / t.fade_in_ms.max(1.0)).min(1.0)
                            }
                            None => 0.0,
                        }
                    };
                    if fade <= 0.0 {
                        continue;
                    }
                    let ease = 1.0 - (1.0 - fade) * (1.0 - fade) * (1.0 - fade);

                    let selected = i == self.selected;
                    let hovered = Some(i) == self.hovered;
                    let emphasized = selected || hovered;

                    let (mut thumb, tex_mix) = match clip.thumb {
                        Thumb::Ready { slot, at, tw, th } => {
                            let m = if !ui {
                                1.0
                            } else {
                                (now.saturating_duration_since(at).as_secs_f32() / THUMB_FADE_S)
                                    .min(1.0)
                            };
                            (
                                Some((slot, tw as f32, th as f32)),
                                1.0 - (1.0 - m) * (1.0 - m),
                            )
                        }
                        _ => (None, 0.0),
                    };
                    // The selected tile shows the hires stream (GPU-downscaled
                    // via mips); hovered tiles show their tile-size atlas lane.
                    let mut live_hires: Option<(f32, f32)> = None;
                    if selected {
                        if let Some(l) = &self.live_sel {
                            // Path (not index) match: indices churn during the
                            // D swap. A skip-respawned stream with no frame yet
                            // keeps showing the texture's last frame — same
                            // clip, so it reads as a freeze then jump.
                            if l.path == clip.path
                                && (l.first_frame.is_some()
                                    || self.hires_shown.as_ref() == Some(&l.path))
                            {
                                live_hires = Some((l.player.w as f32, l.player.h as f32));
                            }
                        }
                    } else if hovered
                        && let Some(live) = &self.live_hover
                        && live.clip == i
                        && live.first_frame.is_some()
                    {
                        thumb = Some((live.slot, live.player.w as f32, live.player.h as f32));
                    }

                    let s = clip.scale * (0.92 + 0.08 * ease);
                    let (wg, hg) = (lay.tile_w * s, lay.tile_h * s);
                    let mut w = wg;
                    let mut h = hg;

                    // Emphasized tiles morph (via the emph spring) toward the
                    // clip's own aspect ratio, capped at max_display_aspect
                    // (pan & scan), sized to *cover* the scaled cell box so no
                    // background peeks out behind portrait clips. The crop
                    // below derives from w/h, so it morphs along.
                    let e = clip.emph.clamp(0.0, 1.0);
                    if e > 0.001
                        && let Some((_, tw, th)) = thumb
                    {
                        let m = t.max_display_aspect.max(1.0);
                        let a = (tw / th).clamp(1.0 / m, m);
                        let (we, he) = if a > wg / hg {
                            (hg * a, hg)
                        } else {
                            (wg, wg / a)
                        };
                        w = wg + (we - wg) * e;
                        h = hg + (he - hg) * e;
                    }
                    let (ox, oy) = self.cell_origin(&lay, col, row);
                    let cx = ox + lay.tile_w * 0.5;
                    let cy = oy + lay.tile_h * 0.5;

                    // Texture source: in the grid, a cycling anim-sheet frame
                    // once available (M6); the static thumb otherwise.
                    // Emphasized tiles never use the sheet: its tiny 16:9
                    // crop-fill frames zoom horribly when the tile morphs to
                    // true aspect, and live video (seek-matched to the static
                    // thumb's frame) would land on different content. The
                    // emphasis morph itself keeps the tile alive until then.
                    let anim_allowed = self.sheets_on() && !emphasized && !self.paused();
                    let anim = if anim_allowed {
                        match clip.anim {
                            Thumb::Ready { slot, at, tw, th } => {
                                Some((slot, at, tw as f32, th as f32))
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };

                    let mut mix = tex_mix;
                    let mut uv2 = [0.0; 4];
                    let mut frame_fade = 0.0;
                    let mut tile_hires = false;
                    let uv = if let Some((lw, lh)) = live_hires {
                        // Crop the hires video frame to the tile's current
                        // (morphing) shape, like the static path below.
                        tile_hires = true;
                        mix = 1.0;
                        let target_a = w / h.max(1.0);
                        let (mut cw, mut ch) = (lw, lh);
                        if lw / lh > target_a {
                            cw = lh * target_a;
                        } else {
                            ch = lw / target_a;
                        }
                        let (hw, hh) =
                            (self.atlas_cfg.hires_w as f32, self.atlas_cfg.hires_h as f32);
                        [
                            ((lw - cw) * 0.5 + 0.5) / hw,
                            ((lh - ch) * 0.5 + 0.5) / hh,
                            (cw - 1.0).max(0.0) / hw,
                            (ch - 1.0).max(0.0) / hh,
                        ]
                    } else if let Some((slot, at, sw, sh)) = anim {
                        self.anim_rendered = true;
                        let cols = self.anim_grid as usize;
                        let frames = (cols * cols) as f32;
                        let (fw, fh) = (sw / cols as f32, sh / cols as f32);
                        // Per-clip phase offset so neighbors don't tick in
                        // lockstep.
                        let phase = (i % (cols * cols)) as f32 / frames;
                        let pos = (anim_t / t.anim_cycle_s.max(0.2) + phase).fract() * frames;
                        let k = (pos as usize).min(cols * cols - 1);

                        let target_a = w / h.max(1.0);
                        let (mut cw, mut ch) = (fw, fh);
                        if fw / fh > target_a {
                            cw = fh * target_a;
                        } else {
                            ch = fw / target_a;
                        }
                        let (ox, oy) = ((fw - cw) * 0.5, (fh - ch) * 0.5);
                        let frame_uv = |kk: usize| {
                            self.atlas_cfg.uv(
                                slot,
                                (kk % cols) as f32 * fw + ox,
                                (kk / cols) as f32 * fh + oy,
                                cw,
                                ch,
                            )
                        };
                        // Crossfade the tail of each frame interval into the
                        // next frame (blended in the shader — two texture taps).
                        let win = t.anim_crossfade.clamp(0.0, 1.0);
                        if win > 0.0 {
                            let ff = pos - k as f32;
                            let f = ((ff - (1.0 - win)) / win).clamp(0.0, 1.0);
                            frame_fade = f * f * (3.0 - 2.0 * f);
                            uv2 = frame_uv((k + 1) % (cols * cols));
                        }
                        let anim_fade = {
                            let m = (now.saturating_duration_since(at).as_secs_f32()
                                / THUMB_FADE_S)
                                .min(1.0);
                            1.0 - (1.0 - m) * (1.0 - m)
                        };
                        // If the static thumb is already showing, swap without
                        // re-fading (no dip to background).
                        mix = if thumb.is_some() {
                            mix.max(anim_fade)
                        } else {
                            anim_fade
                        };
                        frame_uv(k)
                    } else {
                        // UV window into the static thumb, cropped to the tile's
                        // current shape — the shape morphs with the emphasis
                        // spring, so the crop morphs along with it.
                        match thumb {
                            Some((slot, tw, th)) => {
                                let target_a = w / h.max(1.0);
                                let (mut cw, mut ch) = (tw, th);
                                if tw / th > target_a {
                                    cw = th * target_a;
                                } else {
                                    ch = tw / target_a;
                                }
                                self.atlas_cfg
                                    .uv(slot, (tw - cw) * 0.5, (th - ch) * 0.5, cw, ch)
                            }
                            None => [0.0; 4],
                        }
                    };

                    // No random placeholder colors: empty tiles are transparent
                    // (thin grey outline below) and the thumbnail fades in from
                    // nothing. Cloud/unreadable keep their status tints.
                    let (fill_rgb, fill_a) = if clip.cloud {
                        ([0.05, 0.08, 0.13], ease)
                    } else if !clip.readable {
                        ([0.16, 0.05, 0.06], ease)
                    } else if selected {
                        // The selected tile always has a body, even before
                        // its thumbnail exists — but in the window's own
                        // background color: a lighter grey read as a bright
                        // flash whenever a tile waited on live video.
                        (t.background, ease)
                    } else {
                        ([0.0, 0.0, 0.0], ease * mix)
                    };

                    let (sb, hb, eb) = (t.selection_border, t.hover_border, t.empty_border);
                    let (border_color, border_width, radius) = if selected {
                        (
                            [sb[0], sb[1], sb[2], ease],
                            t.selection_border_width,
                            t.selection_corner_radius,
                        )
                    } else if hovered {
                        (
                            [hb[0], hb[1], hb[2], 0.35 * ease],
                            t.hover_border_width,
                            t.corner_radius,
                        )
                    } else if thumb.is_none() && clip.readable && !clip.cloud {
                        ([eb[0], eb[1], eb[2], ease], 1.0, t.corner_radius)
                    } else {
                        ([0.0; 4], 0.0, t.corner_radius)
                    };

                    let tile = Tile {
                        x: cx - w * 0.5,
                        y: cy - h * 0.5,
                        w,
                        h,
                        color: [fill_rgb[0], fill_rgb[1], fill_rgb[2], fill_a],
                        border_color,
                        corner_radius: radius * s,
                        border_width,
                        uv,
                        uv2,
                        frame_fade,
                        tex_mix: mix,
                        hires: tile_hires,
                    };
                    let out = if selected {
                        &mut selected_group
                    } else if hovered {
                        &mut hovered_group
                    } else {
                        &mut tiles
                    };
                    out.push(tile);
                    if clip.cloud && w > 70.0 {
                        push_cloud_badge(out, &tile, ease);
                    }
                    if matches!(clip.thumb, Thumb::Pending) && w > 70.0 {
                        // Selected tiles make the wait obvious: big, centered.
                        push_loading_dots(out, &tile, ease, anim_t, selected);
                    }
                    // Post-skip position flash (in quickview the modal has it).
                    if selected
                        && !self.quickview
                        && let Some((pos, a)) = skip_bar
                    {
                        push_skip_bar(out, &tile, pos, a * ease);
                    }
                }
            }
            tiles.extend(hovered_group);
            tiles.extend(selected_group);
            if modal {
                // First modal frame: this build becomes the frozen backdrop.
                self.frozen_grid = Some((self.viewport.width, self.viewport.height, tiles.clone()));
            }
        }

        // Photos-style reflow: when the column count changes (zoom/resize),
        // the previous layout fades out on top of the new one — never
        // behind a modal, where the frozen backdrop can't reflow and a
        // crossfade would just be invisible churn.
        if modal {
            self.transition = None;
        } else if ui && lay.cols != self.last_cols && !self.last_tiles.is_empty() {
            self.transition = Some((std::mem::take(&mut self.last_tiles), now));
        }
        self.last_cols = lay.cols;
        self.last_tiles = tiles.clone();

        let mut done = false;
        if let Some((old, start)) = &self.transition {
            let f = now.saturating_duration_since(*start).as_secs_f32() * 1000.0
                / t.zoom_fade_ms.max(1.0);
            if f >= 1.0 {
                done = true;
            } else {
                let fade = (1.0 - f) * (1.0 - f); // ease-out
                tiles.extend(old.iter().map(|tl| {
                    let mut tl = *tl;
                    tl.color[3] *= fade;
                    tl.border_color[3] *= fade;
                    tl
                }));
            }
        }
        if done {
            self.transition = None;
        }

        // Quickview (PLAN.md §6 level 3, internal): blur + dim everything
        // and show the selected clip large, centered, playing via the live
        // slot. Arrows keep working — the modal follows the selection.
        let mut blur = None;
        // Skipped entirely under fullview: everything here would sit
        // behind its opaque black backdrop.
        if self.quickview
            && !self.fullview
            && let Some(clip) = self.clips.get(self.selected)
        {
            let fade = if !ui {
                1.0
            } else {
                (self.quickview_at.elapsed().as_secs_f32() * 1000.0 / t.quickview_fade_ms.max(1.0))
                    .min(1.0)
            };
            let (vw, vh) = (self.viewport.width, self.viewport.height);
            if t.quickview_blur >= 0.5 {
                blur = Some(Blur {
                    split: tiles.len(),
                    levels: t.quickview_blur.round() as u32,
                    fade,
                });
            }
            let full = |x, y, w, h, color| Tile {
                x,
                y,
                w,
                h,
                color,
                border_color: [0.0; 4],
                corner_radius: 0.0,
                border_width: 0.0,
                uv: [0.0; 4],
                uv2: [0.0; 4],
                frame_fade: 0.0,
                tex_mix: 0.0,
                hires: false,
            };
            tiles.push(full(
                0.0,
                0.0,
                vw,
                vh,
                [0.0, 0.0, 0.0, t.quickview_dim.clamp(0.0, 1.0) * fade],
            ));

            // The modal shows the same hires stream the tile plays —
            // already running, already sharp, no handoff. Static thumb
            // stands in for the brief first-frame window; a skip
            // respawn keeps the texture's last frame instead; and
            // during the D swap the shielded stream stays up while
            // the selection index is in flux.
            let src = self.selected_video_src(clip);
            // The video sits above the filmstrip (geometry shared with
            // the pointer hit-tests via quickview_video_rect).
            let (_, chip_h, strip_y) = self.strip_geom();
            let avail_h = (strip_y - 18.0).max(60.0);
            if let (Some((uv, _, _, hires)), Some((rx, ry, rw, rh))) =
                (src, self.quickview_video_rect())
            {
                let video = Tile {
                    x: rx,
                    y: ry,
                    w: rw,
                    h: rh,
                    color: [0.0, 0.0, 0.0, fade],
                    border_color: [0.0; 4],
                    corner_radius: t.selection_corner_radius,
                    border_width: 0.0,
                    uv,
                    uv2: [0.0; 4],
                    frame_fade: 0.0,
                    tex_mix: fade,
                    hires,
                };
                tiles.push(video);
                // Seekbar along the video's bottom: revealed by pointer
                // motion (fades after a short idle) or the skip flash;
                // thickens under the pointer; click/drag scrubs.
                let bar_a = {
                    let flash = skip_bar.map(|(_, a)| a).unwrap_or(0.0);
                    self.seekbar_alpha().max(flash) * fade
                };
                self.push_seekbar(&mut tiles, rx, rw, clip, bar_a);
            } else {
                // Nothing decoded yet: big dots in the middle.
                let stage = full(
                    (vw - 300.0) * 0.5,
                    (avail_h - 100.0) * 0.5,
                    300.0,
                    100.0,
                    [0.0; 4],
                );
                push_loading_dots(&mut tiles, &stage, fade, anim_t, true);
            }

            // Filmstrip: neighbors along the bottom, selected centered,
            // sliding on the keyboard chase spring. Chips keep a FIXED
            // height and take each clip's true aspect for width —
            // portrait clips are narrower, never taller — so positions
            // come from the cumulative strip_layout walk. Foreground
            // layer — in quickview, actions live here; the grid is
            // backdrop. Z-order like the grid: hovered chip above its
            // neighbors, selected chip on top (both scale up and
            // overlap).
            let mut sel_chip: Option<Tile> = None;
            let mut hover_chip: Option<Tile> = None;
            for (i, cx, cw_i) in self.strip_layout(self.strip_pos) {
                let c = &self.clips[i];
                let sel = i == self.selected;
                let hov = !sel && self.strip_hover == Some(i);
                let s = if sel {
                    t.strip_selection_scale
                } else if hov {
                    t.strip_hover_scale
                } else {
                    1.0
                };
                let (w, h) = (cw_i * s, chip_h * s);
                // The hovered chip plays its lane's video (tile-size,
                // in the atlas Live slot); everything else shows its
                // thumb — cropped only as far as the aspect clamp asks
                // (usually not at all: the chip IS the clip's shape).
                let live = self
                    .live_hover
                    .as_ref()
                    .filter(|l| hov && l.clip == i && l.first_frame.is_some())
                    .map(|l| (l.slot, l.player.w as f32, l.player.h as f32));
                let src = live.or(match c.thumb {
                    Thumb::Ready { slot, tw, th, .. } => Some((slot, tw as f32, th as f32)),
                    _ => None,
                });
                let (uv, has_tex) = match src {
                    Some((slot, tw, th)) => {
                        let target_a = cw_i / chip_h.max(1.0);
                        let (mut cw, mut ch) = (tw, th);
                        if tw / th > target_a {
                            cw = th * target_a;
                        } else {
                            ch = tw / target_a;
                        }
                        (
                            self.atlas_cfg
                                .uv(slot, (tw - cw) * 0.5, (th - ch) * 0.5, cw, ch),
                            true,
                        )
                    }
                    None => ([0.0; 4], false),
                };
                let sb = t.selection_border;
                let hb = t.hover_border;
                let (border_color, border_width) = if sel {
                    ([sb[0], sb[1], sb[2], fade], t.strip_border_width)
                } else if hov {
                    (
                        [hb[0], hb[1], hb[2], fade],
                        (t.strip_border_width * 0.5).max(1.0),
                    )
                } else {
                    ([0.30, 0.30, 0.34, 0.55 * fade], 1.0)
                };
                let tile = Tile {
                    x: cx - w * 0.5,
                    y: strip_y + (chip_h - h) * 0.5,
                    w,
                    h,
                    // Unloaded chips show the window background, not a
                    // lighter grey — a chip waiting on its thumb (or its
                    // hover video) must not flash bright.
                    color: [t.background[0], t.background[1], t.background[2], fade],
                    border_color,
                    corner_radius: t.strip_corner_radius,
                    border_width,
                    uv,
                    uv2: [0.0; 4],
                    frame_fade: 0.0,
                    tex_mix: if has_tex { fade } else { 0.0 },
                    hires: false,
                };
                if sel {
                    sel_chip = Some(tile);
                } else if hov {
                    hover_chip = Some(tile);
                } else {
                    tiles.push(tile);
                }
            }
            tiles.extend(hover_chip);
            tiles.extend(sel_chip);
        }

        // Fullview (internal `fullview` action): the selected clip fills
        // the whole window, letterboxed on an opaque black backdrop — no
        // filmstrip or blur, but the same pointer-revealed seekbar as the
        // quickview modal. Reuses the same hires stream the tile plays, so
        // it opens instantly with no handoff. Drawn over everything
        // (including any quickview underneath); Esc/tab exits.
        if self.fullview {
            let (vw, vh) = (self.viewport.width, self.viewport.height);
            let full = |x, y, w, h, color| Tile {
                x,
                y,
                w,
                h,
                color,
                border_color: [0.0; 4],
                corner_radius: 0.0,
                border_width: 0.0,
                uv: [0.0; 4],
                uv2: [0.0; 4],
                frame_fade: 0.0,
                tex_mix: 0.0,
                hires: false,
            };
            // Opaque black covers the grid (and any quickview) behind it.
            tiles.push(full(0.0, 0.0, vw, vh, [0.0, 0.0, 0.0, 1.0]));
            if let Some(clip) = self.clips.get(self.selected) {
                if let (Some((uv, _, _, hires)), Some((rx, ry, rw, rh))) =
                    (self.selected_video_src(clip), self.fullview_video_rect())
                {
                    tiles.push(Tile {
                        x: rx,
                        y: ry,
                        w: rw,
                        h: rh,
                        color: [0.0, 0.0, 0.0, 1.0],
                        border_color: [0.0; 4],
                        corner_radius: 0.0,
                        border_width: 0.0,
                        uv,
                        uv2: [0.0; 4],
                        frame_fade: 0.0,
                        tex_mix: 1.0,
                        hires,
                    });
                    // The chapter bar owns the bottom edge while up: the
                    // seekbar fades out as the bar slides in.
                    let chapter_slide = self.chapters.as_ref().map(|b| b.slide).unwrap_or(0.0);
                    let bar_a = self
                        .seekbar_alpha()
                        .max(skip_bar.map(|(_, a)| a).unwrap_or(0.0))
                        * (1.0 - chapter_slide);
                    self.push_seekbar(&mut tiles, rx, rw, clip, bar_a);
                } else {
                    // Nothing decoded yet: loading dots on the black stage.
                    let stage = full(
                        (vw - 300.0) * 0.5,
                        (vh - 100.0) * 0.5,
                        300.0,
                        100.0,
                        [0.0; 4],
                    );
                    push_loading_dots(&mut tiles, &stage, 1.0, anim_t, true);
                }
            }
        }

        // Fullview chapter bar (`chapter_mode`): filmstrip-style chips
        // sliding up from the bottom edge — real chapter starts or
        // synthesized checkpoints, imaged from the clip's cached anim
        // sheet (the same frames the seekbar storyboard shows, so chips
        // appear as fast as one disk-cached decode). Scrolls sideways
        // like the quickview filmstrip; the playing chapter wears the
        // selection border; clicking seeks and sends the bar back down.
        if let Some(bar) = self.chapters.clone()
            && bar.slide > 0.005
            && let Some((anim_src, anim_failed)) = self.clips.get(self.selected).map(|c| {
                (
                    match c.anim {
                        Thumb::Ready { slot, tw, th, .. } => Some((slot, tw as f32, th as f32)),
                        _ => None,
                    },
                    matches!(c.anim, Thumb::Failed),
                )
            })
        {
            let vw = self.viewport.width;
            let a = bar.slide; // the slide doubles as the bar's alpha
            let (_, chh, base_y) = self.strip_geom();
            // Fixed height, the CLIP's true aspect for width: portrait
            // clips get narrower chips, never taller ones.
            let cw = self.chapter_chip_w();
            let strip_y = base_y + (1.0 - bar.slide) * (chh + 44.0);
            match bar.times.as_deref() {
                // Probe still out: a few small dots low-center.
                None => {
                    let stage = Tile {
                        x: (vw - 120.0) * 0.5,
                        y: strip_y,
                        w: 120.0,
                        h: chh,
                        color: [0.0; 4],
                        border_color: [0.0; 4],
                        corner_radius: 0.0,
                        border_width: 0.0,
                        uv: [0.0; 4],
                        uv2: [0.0; 4],
                        frame_fade: 0.0,
                        tex_mix: 0.0,
                        hires: false,
                    };
                    push_loading_dots(&mut tiles, &stage, a, anim_t, true);
                    self.wake(0.3);
                }
                Some(times) if !times.is_empty() => {
                    let n = times.len();
                    let step = cw + t.strip_gap;
                    let center = bar.pos;
                    let half = (vw / step) as i64 / 2 + 1;
                    let hover = self.chapter_chip_at(self.cursor.0, self.cursor.1);
                    let current = self.current_chapter(&bar);
                    let d = bar.duration.filter(|d| *d > 0.05);
                    let (sb, hb, bg) = (t.selection_border, t.hover_border, t.background);
                    let mut cur_chip: Option<Tile> = None;
                    let mut hov_chip: Option<Tile> = None;
                    let mut dot_stages: Vec<Tile> = Vec::new();
                    for di in -half..=half {
                        let idx = center.round() as i64 + di;
                        if idx < 0 || idx as usize >= n {
                            continue;
                        }
                        let i = idx as usize;
                        let cur = current == Some(i);
                        let hov = !cur && hover == Some(i);
                        let s = if cur {
                            t.strip_selection_scale
                        } else if hov {
                            t.strip_hover_scale
                        } else {
                            1.0
                        };
                        let (w, h) = (cw * s, chh * s);
                        let cx = vw * 0.5 + (i as f32 - center) * step;
                        // The nearest anim-sheet cell to this chapter's
                        // timestamp, center-cropped from the 16:9 cell to
                        // the chip's (= clip's) aspect so nothing ever
                        // stretches. Landscape clips crop ~nothing;
                        // portrait chips show the cell's center band.
                        let uv = match (anim_src, d) {
                            (Some((slot, tw, th)), Some(d)) => {
                                let g = self.anim_grid.max(1) as usize;
                                let cells = (g * g) as f32;
                                let f = (times[i] / d).clamp(0.0, 1.0) as f32;
                                let k = ((f * cells) as usize).min(g * g - 1);
                                let (fw, fh) = (tw / g as f32, th / g as f32);
                                let target_a = cw / chh.max(1.0);
                                let (mut cw2, mut ch2) = (fw, fh);
                                if fw / fh > target_a {
                                    cw2 = fh * target_a;
                                } else {
                                    ch2 = fw / target_a;
                                }
                                Some(self.atlas_cfg.uv(
                                    slot,
                                    (k % g) as f32 * fw + (fw - cw2) * 0.5,
                                    (k / g) as f32 * fh + (fh - ch2) * 0.5,
                                    cw2,
                                    ch2,
                                ))
                            }
                            _ => None,
                        };
                        let (border_color, border_width) = if cur {
                            ([sb[0], sb[1], sb[2], a], t.strip_border_width)
                        } else if hov {
                            (
                                [hb[0], hb[1], hb[2], a],
                                (t.strip_border_width * 0.5).max(1.0),
                            )
                        } else {
                            ([0.30, 0.30, 0.34, 0.55 * a], 1.0)
                        };
                        let tile = Tile {
                            x: cx - w * 0.5,
                            y: strip_y + (chh - h) * 0.5,
                            w,
                            h,
                            color: [bg[0], bg[1], bg[2], a],
                            border_color,
                            corner_radius: t.strip_corner_radius,
                            border_width,
                            uv: uv.unwrap_or([0.0; 4]),
                            uv2: [0.0; 4],
                            frame_fade: 0.0,
                            tex_mix: if uv.is_some() { a } else { 0.0 },
                            hires: false,
                        };
                        if cur {
                            cur_chip = Some(tile);
                        } else if hov {
                            hov_chip = Some(tile);
                        } else {
                            tiles.push(tile);
                        }
                        // Sheet still generating: dots on the chip
                        // (after the elevated chips so they stay visible).
                        if uv.is_none() && !anim_failed {
                            dot_stages.push(tile);
                            self.wake(0.3);
                        }
                    }
                    tiles.extend(hov_chip);
                    tiles.extend(cur_chip);
                    for stage in &dot_stages {
                        push_loading_dots(&mut tiles, stage, a, anim_t, false);
                    }
                }
                _ => {}
            }
        }

        // Background-jobs indicator: a hairline progress bar, bottom-right.
        // Drawn last — above the quickview dim — so "still working" stays
        // visible whenever it's true. Lingers a moment when the batch
        // finishes, then fades.
        let pending = self.jobs_total.saturating_sub(self.jobs_done);
        if self.jobs_total > 0 {
            let fade = if pending > 0 {
                self.jobs_finished_at = None;
                1.0
            } else {
                let at = *self.jobs_finished_at.get_or_insert(now);
                1.0 - (now.saturating_duration_since(at).as_secs_f32() / 0.7).min(1.0)
            };
            if fade <= 0.0 {
                self.jobs_total = 0;
                self.jobs_done = 0;
                self.jobs_finished_at = None;
            } else {
                let progress = self.jobs_done as f32 / self.jobs_total as f32;
                let (bw, bh) = (84.0, 6.0);
                let bx = self.viewport.width - bw - 24.0;
                let by = self.viewport.height - bh - 22.0;
                let bar = |x: f32, w: f32, a: f32| Tile {
                    x,
                    y: by,
                    w,
                    h: bh,
                    color: [0.85, 0.85, 0.9, a * fade],
                    border_color: [0.0; 4],
                    corner_radius: bh * 0.5,
                    border_width: 0.0,
                    uv: [0.0; 4],
                    uv2: [0.0; 4],
                    frame_fade: 0.0,
                    tex_mix: 0.0,
                    hires: false,
                };
                tiles.push(bar(bx, bw, 0.03)); // track
                tiles.push(bar(bx, (bw * progress).max(bh), 0.45)); // fill
            }
        }

        Frame {
            clear: t.background,
            tiles,
            uploads: Vec::new(),
            hires_upload: None,
            blur,
            animating: true,
            redraw_at: None,
        }
    }
}

impl App for Switchblade {
    fn atlas(&self) -> AtlasCfg {
        self.atlas_cfg
    }

    fn event(&mut self, event: InputEvent) {
        // Any input keeps the loop awake long enough for settle timers
        // (live start, hover) to fire.
        self.wake(0.6);
        match event {
            InputEvent::Key { key, .. } => self.key(key),
            InputEvent::Scroll { dx, dy } => {
                // Chapter bar up: the wheel pans the chapter strip like
                // the quickview filmstrip — but panning only (chapters
                // seek on click, never on scroll), clamped so the row's
                // ends stay reachable and nothing moves underneath.
                if self.chapters.as_ref().is_some_and(|b| b.open) {
                    let n = self
                        .chapters
                        .as_ref()
                        .and_then(|b| b.times.as_ref())
                        .map(|ts| ts.len())
                        .unwrap_or(0);
                    let step = self.chapter_chip_w() + self.tuning.strip_gap;
                    if n > 0 && step > 1.0 {
                        let d = if dx.abs() > dy.abs() { dx } else { dy };
                        let (lo, hi) = self.chapter_pos_bounds(n);
                        let sens = self.tuning.strip_scroll_sensitivity;
                        if let Some(b) = &mut self.chapters {
                            b.target = (b.target - d * sens / step).clamp(lo, hi);
                        }
                    }
                    return;
                }
                if self.chapters.is_some() {
                    return; // bar mid-slide: nothing scrolls under it
                }
                if self.fullview {
                    return; // the grid is hidden — nothing to scroll
                }
                if self.quickview {
                    // Wheel/trackpad scrubs the filmstrip: the strip slides
                    // freely under the gesture and the selection commits to
                    // the nearest chip — the snap spring then centers it,
                    // so it reads as magnetic, chip-by-chip flow. The grid
                    // backdrop never pans under the modal.
                    let (cw, _, _) = self.strip_geom();
                    let step = cw + self.tuning.strip_gap;
                    let d = if dx.abs() > dy.abs() { dx } else { dy };
                    let n = self.clips.len();
                    if n > 0 && step > 1.0 {
                        self.strip_target = (self.strip_target
                            - d * self.tuning.strip_scroll_sensitivity / step)
                            .clamp(0.0, (n - 1) as f32);
                        self.last_scroll_event = Instant::now();
                        let i = self.strip_target.round() as usize;
                        if i != self.selected {
                            self.selected = i;
                            self.sel_changed_at = Instant::now();
                            self.pending_reselect = None; // scroll outranks the D reselect
                            self.scroll_to_selected();
                        }
                    }
                    return;
                }
                let d = -dy * self.tuning.pan_sensitivity;
                self.scroll_target += d;
                self.scroll_vel = self.scroll_vel * 0.7 + d * 60.0 * 0.3;
                self.last_scroll_event = Instant::now();
                self.chase = self.tuning.snap_strength;
            }
            InputEvent::Pinch { delta } => {
                // Zooming a frozen/hidden grid is invisible churn — the
                // gesture belongs to the grid, not the modals.
                if self.quickview || self.fullview {
                    return;
                }
                let factor = 1.0 + delta * self.tuning.pinch_sensitivity;
                self.set_zoom(self.zoom_target * factor.max(0.01));
            }
            InputEvent::CursorMoved { x, y } => {
                self.cursor = (x, y);
                if self.scrubbing {
                    // Drag-scrub: coarse keyframe hops track the pointer
                    // (the decoder coalesces to the newest target); the
                    // exact landing waits for release.
                    self.seekbar_seen = Some(Instant::now());
                    if let Some(f) = self.seekbar_frac(x) {
                        self.scrub_seek(f, false);
                    }
                } else if self.active_video_rect().is_some_and(|(vx, vy, vw, vh)| {
                    (vx..=vx + vw).contains(&x) && (vy..=vy + vh + 12.0).contains(&y)
                }) {
                    self.seekbar_seen = Some(Instant::now());
                    // Stay awake through the idle-hide and the fade tail.
                    self.wake(
                        self.tuning.seekbar_hide_s + self.tuning.seekbar_fade_ms / 1000.0 + 0.2,
                    );
                }
            }
            InputEvent::Focus { focused } => {
                self.focused = focused;
            }
            InputEvent::MouseDown { x, y } => {
                // In fullview: an open chapter bar grabs first — a chip
                // click seeks the video to that chapter and sends the
                // bar back down; any other click just closes the bar
                // (fullview stays). Then the seekbar (click = seek,
                // hold = scrub); any other click exits — the grid it
                // would select is hidden behind the black backdrop.
                if self.fullview {
                    if self.chapters.as_ref().is_some_and(|b| b.open) {
                        if let Some(i) = self.chapter_chip_at(x, y) {
                            self.chapter_seek(i);
                        }
                        self.close_chapter_bar();
                        return;
                    }
                    if let Some(f) = self.seekbar_hit(x, y) {
                        self.scrubbing = true;
                        self.seekbar_seen = Some(Instant::now());
                        self.scrub_seek(f, false);
                        return;
                    }
                    self.fullview = false;
                    return;
                }
                // In quickview: the seekbar grabs first (click = seek,
                // hold = scrub), then a filmstrip chip click selects it;
                // anywhere else closes. In the grid: click selects, click
                // on the selection opens quickview.
                if self.quickview {
                    if let Some(f) = self.seekbar_hit(x, y) {
                        self.scrubbing = true;
                        self.seekbar_seen = Some(Instant::now());
                        self.scrub_seek(f, false);
                        return;
                    }
                    if let Some(i) = self.strip_chip_at(x, y) {
                        if i != self.selected {
                            self.selected = i;
                            self.sel_changed_at = Instant::now();
                            self.pending_reselect = None; // click outranks the D reselect
                            self.scroll_to_selected();
                        }
                    } else {
                        self.quickview = false;
                    }
                    return;
                }
                let lay = self.layout();
                if let Some(i) = self.tile_at(&lay, x, y) {
                    if i == self.selected {
                        self.quickview = true;
                        self.quickview_at = Instant::now();
                        self.strip_pos = self.selected as f32;
                        self.strip_target = self.strip_pos;
                    } else {
                        self.selected = i;
                        self.sel_changed_at = Instant::now();
                        self.pending_reselect = None; // click outranks the D reselect
                    }
                }
            }
            InputEvent::MouseUp { x, .. } => {
                if self.scrubbing {
                    self.scrubbing = false;
                    if let Some(f) = self.seekbar_frac(x) {
                        self.scrub_seek(f, true); // exact landing on release
                    }
                }
            }
        }
    }

    fn frame(&mut self, dt: f32, viewport: Viewport) -> Frame {
        self.viewport = viewport;
        if let Some(f) = &mut self.tuning_file
            && let Some(cfg) = f.poll()
        {
            self.tuning = cfg.tuning;
            self.keymap = cfg.keymap;
        }
        self.drain_ingest();
        // The chapter bar is a fullview add-on describing one clip: drop
        // it the moment fullview exits or the selection leaves its clip
        // (movement keys, random jump, D swap churn) — no slide-out, the
        // whole context changed.
        if self.chapters.as_ref().is_some_and(|b| {
            !self.fullview
                || self
                    .clips
                    .get(self.selected)
                    .is_none_or(|c| c.path != b.path)
        }) {
            self.chapters = None;
        }
        // Fullview pre-warms the chapter probe for the clip on screen,
        // so pressing g finds the plan already cached (the anim sheet
        // pre-warms too, via request_quickview_sheet's fullview gate) —
        // but only after the watched stream is up (prewarm_ok).
        if self.fullview && !self.demo && self.prewarm_ok() {
            let path = self
                .clips
                .get(self.selected)
                .filter(|c| c.readable && !c.cloud)
                .map(|c| c.path.clone());
            if let Some(path) = path
                && !self.chapter_probe.contains_key(&path)
            {
                self.chapter_probe.insert(path.clone(), None);
                self.media.request_chapters(path);
            }
        }
        self.tick_skip_timer();
        self.step(dt);
        let lay = self.layout();
        self.request_visible_thumbs(&lay);
        self.request_quickview_sheet();
        let mut uploads = self.drain_media(&lay);
        self.update_live(&lay, &mut uploads);
        self.update_title();
        let mut frame = self.build_frame();
        frame.uploads = uploads;
        frame.hires_upload = self.hires_frame.take();
        // Idle throttling: with nothing VISUALLY in motion — springs
        // settled, no sheets cycling, no live video, no fade in flight —
        // the loop drops to a slow tick. Pending background jobs and an
        // open ingest producer deliberately do NOT keep the loop hot
        // (P0.2): their worker threads fire `waker` per delivery, so each
        // completion repaints once, and the idle tick still services them
        // at 10Hz as a safety net.
        if log::log_enabled!(log::Level::Debug) {
            // Atlas sizing evidence (P0.1/P0.5): actual occupancy vs the
            // zone's demand — a static per in-zone clip, an anim sheet
            // only when sheets actually cycle (animation level `full` +
            // the `a` toggle), plus the live/hover lanes.
            let used = self.slots.iter().filter(|s| s.is_some()).count();
            let (first, last) = self.visible_rows(&lay, PREFETCH_ROWS);
            let per_clip = 1 + usize::from(self.sheets_on());
            let demand =
                ((last - first + 1) * lay.cols * per_clip).min(self.clips.len() * per_clip) + 2;
            self.redraw_stats.slots(used, demand);
        }
        let motion = self.motion;
        let sheets = self.anim_rendered;
        let transition = self.transition.is_some();
        let timer = Instant::now() < self.wake_until;
        // Live video no longer forces display-rate redraws (P1.4): with
        // the UI settled, the next queued frame's due time becomes a
        // one-shot deadline — a 30fps clip presents ~30 times a second
        // on a 120Hz display instead of 120. A parked selected stream
        // (offscreen, undrained) reports nothing; a dry queue reports
        // nothing either (the reader's push-notify wakes the loop when
        // the next frame lands).
        let sel_due = self
            .live_sel
            .as_ref()
            .filter(|_| !self.sel_parked)
            .and_then(|l| l.player.next_due());
        let hover_due = self.live_hover.as_ref().and_then(|l| l.player.next_due());
        frame.redraw_at = match (sel_due, hover_due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        frame.animating = motion || sheets || transition || timer;
        self.redraw_stats.record(
            motion,
            sheets,
            transition,
            frame.redraw_at.is_some(),
            timer,
            frame.animating,
        );
        frame
    }

    fn commands(&mut self) -> Vec<WindowCommand> {
        std::mem::take(&mut self.cmds)
    }

    fn waker(&self) -> Waker {
        self.waker.clone()
    }
}

/// A little cloud in the tile's bottom-right corner, built from two circles
/// and a rounded bar — no icon assets, no text stack, just tiles.
fn push_cloud_badge(out: &mut Vec<Tile>, tile: &Tile, ease: f32) {
    let color = [0.62, 0.72, 0.88, ease];
    let bx = tile.x + tile.w - 40.0;
    let by = tile.y + tile.h - 26.0;
    let part = |x: f32, y: f32, w: f32, h: f32| Tile {
        x,
        y,
        w,
        h,
        color,
        border_color: [0.0; 4],
        corner_radius: h * 0.5,
        border_width: 0.0,
        uv: [0.0; 4],
        uv2: [0.0; 4],
        frame_fade: 0.0,
        tex_mix: 0.0,
        hires: false,
    };
    out.push(part(bx + 2.0, by + 4.0, 13.0, 13.0)); // small bump
    out.push(part(bx + 9.0, by, 16.0, 16.0)); // big bump
    out.push(part(bx, by + 7.0, 28.0, 10.0)); // base bar
}

/// A slim playback-position bar along a tile's bottom edge — the short
/// flash after a `[`/`]` skip. Dark track under a bright fill so it reads
/// over any video content.
fn push_skip_bar(out: &mut Vec<Tile>, tile: &Tile, pos: f32, alpha: f32) {
    let pad = 12.0_f32.min(tile.w * 0.06).max(4.0);
    let bw = (tile.w - pad * 2.0).max(8.0);
    let bh = 3.0;
    let by = tile.y + tile.h - bh - pad;
    let bar = |x: f32, w: f32, color: [f32; 4]| Tile {
        x,
        y: by,
        w,
        h: bh,
        color,
        border_color: [0.0; 4],
        corner_radius: bh * 0.5,
        border_width: 0.0,
        uv: [0.0; 4],
        uv2: [0.0; 4],
        frame_fade: 0.0,
        tex_mix: 0.0,
        hires: false,
    };
    let bx = tile.x + pad;
    out.push(bar(bx, bw, [0.08, 0.08, 0.10, 0.55 * alpha]));
    out.push(bar(
        bx,
        (bw * pos).max(bh),
        [0.95, 0.95, 0.98, 0.95 * alpha],
    ));
}

/// Three pulsing dots while a thumbnail is generating: each breathes from
/// near-transparent up to opaque near-white. Small in the bottom-right
/// corner normally; big and centered when the tile is selected.
fn push_loading_dots(out: &mut Vec<Tile>, tile: &Tile, ease: f32, t: f32, big: bool) {
    let (d, gap) = if big { (13.0, 11.0) } else { (5.0, 4.0) };
    let total_w = 3.0 * d + 2.0 * gap;
    let (bx, by) = if big {
        (
            tile.x + (tile.w - total_w) * 0.5,
            tile.y + (tile.h - d) * 0.5,
        )
    } else {
        (tile.x + tile.w - total_w - 10.0, tile.y + tile.h - d - 10.0)
    };
    for k in 0..3 {
        let wave = 0.5 + 0.5 * (t * 4.5 - k as f32 * 0.9).sin();
        let pulse = wave * wave; // sharpen: dwell near-dark, peak bright
        out.push(Tile {
            x: bx + k as f32 * (d + gap),
            y: by,
            w: d,
            h: d,
            color: [0.94, 0.94, 0.97, (0.04 + 0.96 * pulse) * ease],
            border_color: [0.0; 4],
            corner_radius: d * 0.5,
            border_width: 0.0,
            uv: [0.0; 4],
            uv2: [0.0; 4],
            frame_fade: 0.0,
            tex_mix: 0.0,
            hires: false,
        });
    }
}

/// Synthesized chapter-checkpoint count for a clip with no real chapters:
/// none under a minute (the bar just doesn't show), 4 over a minute
/// (every 25%), 8 over three minutes, 10 over ten. Starts land at k/n of
/// the duration, so chapter 1 is always the clip's opening.
fn checkpoint_count(d: f64) -> usize {
    if d > 600.0 {
        10
    } else if d > 180.0 {
        8
    } else if d >= 60.0 {
        4
    } else {
        0
    }
}

/// Cheap non-crypto randomness (jump_random / shuffle_library) with no new
/// dependency: std's HashMap hasher is seeded from system entropy per
/// instance, so one finish() makes a seed; xorshift stretches it.
fn rng_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
        | 1 // xorshift must never see zero
}

fn next_rand(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the app loop headlessly until `cond` holds (or time out).
    fn pump_until(app: &mut Switchblade, cond: impl Fn(&Switchblade) -> bool) -> bool {
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        // Generous: media tests run in parallel and contend for ffmpeg
        // spawns (a cold cache pays clip gen + thumb + live spawn all at
        // once); the loop returns the moment the condition holds.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let _ = app.frame(0.016, vp);
            if cond(app) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// `D` rebuilds the library from the selected clip's parent directory
    /// and the clip finds itself again once the listing streams in.
    #[test]
    fn open_parent_swaps_to_siblings_and_reselects() {
        let dir = std::env::temp_dir().join("sb_app_parent_swap_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["a.mp4", "b.mp4", "c.mp4"] {
            std::fs::write(dir.join(f), b"").unwrap();
        }

        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None), // no live decoders in tests
            inputs: vec![dir.join("b.mp4")],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        assert!(
            pump_until(&mut app, |a| a.clips.len() == 1),
            "single-file ingest"
        );
        assert_eq!(app.selected, 0);

        app.event(InputEvent::Key {
            key: Key::Char('D'),
            repeat: false,
        });
        assert!(
            pump_until(&mut app, |a| a.clips.len() == 3
                && a.pending_reselect.is_none()),
            "siblings swap"
        );
        assert_eq!(
            app.clips[app.selected].path,
            dir.join("b.mp4"),
            "the clip re-finds itself among its siblings"
        );
    }

    /// Pending background jobs must not force continuous rendering: the
    /// gen sweep can run for hours on a big library while the grid is
    /// static — each completion wakes the loop via `Waker` instead
    /// (PERFORMANCE-TASKS.md P0.2).
    #[test]
    fn background_jobs_do_not_force_continuous_animation() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        app.jobs_total = 500;
        app.jobs_done = 3;
        app.wake_until = Instant::now(); // skip the startup grace period
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let idle = (0..600).any(|_| !app.frame(0.016, vp).animating);
        assert!(
            idle,
            "pending background jobs alone kept Frame.animating true"
        );
        assert!(app.jobs_total > app.jobs_done, "jobs still pending");
    }

    /// An open-but-quiet ingest producer (a pipe that stays open, a slow
    /// stdin feeder) must not keep the GPU presenting; arriving items
    /// still ingest on the frame their send wakes (P0.2).
    #[test]
    fn open_ingest_pipe_does_not_force_continuous_animation() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        let (tx, rx) = std::sync::mpsc::channel();
        app.rx = Some(rx);
        app.wake_until = Instant::now();
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let idle = (0..600).any(|_| !app.frame(0.016, vp).animating);
        assert!(idle, "an open, idle ingest pipe kept Frame.animating true");

        // The pipe is still serviced: a late arrival lands next frame…
        let before = app.clips.len();
        tx.send(ingest::Ingested {
            path: PathBuf::from("late/arrival.mp4"),
            readable: false, // no media jobs in this test
            cloud: false,
        })
        .unwrap();
        let _ = app.frame(0.016, vp);
        assert_eq!(app.clips.len(), before + 1, "late arrival ingested");

        // …and producer exit closes out ingest state.
        drop(tx);
        let _ = app.frame(0.016, vp);
        assert!(app.rx.is_none(), "disconnect noticed without a hot loop");
    }

    /// `]` seeks the selected stream in place — the SAME decoder jumps
    /// (no respawn: `first_frame` survives), the flash bar arms, and
    /// frames from the new offset keep flowing. Chained presses are just
    /// more seeks on the same stream. Needs ffmpeg (thumb + live decode)
    /// — skipped quietly when it's not on PATH.
    #[test]
    fn skip_seeks_the_selected_stream_in_place() {
        let have_ffmpeg = std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !have_ffmpeg {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_app_skip_test");
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("skip.mp4");
        if !clip.exists() {
            let ok = std::process::Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=8:size=320x180:rate=30")
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

        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::Normal),
            inputs: vec![clip],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        assert!(
            pump_until(&mut app, |a| a
                .live_sel
                .as_ref()
                .is_some_and(|l| l.first_frame.is_some())),
            "selected live stream never started"
        );
        let before = app.live_sel.as_ref().unwrap().position();

        app.skip(true, Some(0.25)); // 8s clip: +2s
        let l = app.live_sel.as_ref().expect("stream survives the skip");
        assert!(
            l.first_frame.is_some(),
            "in-place seek must not respawn the decoder"
        );
        let target = l.position();
        assert!(
            target > before + 1.5,
            "expected a forward jump, position {target} from {before}"
        );
        assert!(app.skip_bar().is_some(), "the flash bar arms on skip");
        // The same stream delivers from the new offset (exact seek).
        assert!(
            pump_until(&mut app, |a| a.live_sel.as_ref().is_some_and(|l| {
                l.player.buffered() > 0 || (l.position() - target).abs() < 0.5
            })),
            "no frames after the seek"
        );

        // Chained press: same decoder again, further along.
        app.skip(true, Some(0.25));
        let l = app.live_sel.as_ref().expect("stream survives chained skip");
        assert!(l.first_frame.is_some(), "chained skip must not respawn");
        assert!(
            l.position() > target + 1.5,
            "chained skip advances from the previous target, position {}",
            l.position()
        );
    }

    /// A pre-filled ingest backlog drains under the per-frame budget —
    /// no single frame stalls appending thousands of clips — while the
    /// full set still lands, in source order, over later frames (P0.3).
    #[test]
    fn ingest_drains_with_a_per_frame_budget() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        let (tx, rx) = std::sync::mpsc::channel();
        for i in 0..1000 {
            tx.send(ingest::Ingested {
                path: PathBuf::from(format!("bulk/{i:04}.mp4")),
                readable: false, // no media jobs in this test
                cloud: false,
            })
            .unwrap();
        }
        app.rx = Some(rx);
        let before = app.clips.len();
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let _ = app.frame(0.016, vp);
        assert_eq!(
            app.clips.len() - before,
            INGEST_DRAIN_BUDGET,
            "one frame takes exactly the budget, deferring the rest"
        );
        for _ in 0..10 {
            let _ = app.frame(0.016, vp);
        }
        assert_eq!(app.clips.len() - before, 1000, "backlog fully lands");
        assert!(
            app.clips[before..].iter().enumerate().all(|(i, c)| {
                c.path.as_path() == std::path::Path::new(&format!("bulk/{i:04}.mp4"))
            }),
            "streamed order preserved across budget boundaries"
        );
    }

    /// A live lane whose decoder reports `failed()` is reaped — the tile
    /// falls back to its static thumbnail — and the path enters a retry
    /// cooldown so the settle logic doesn't respawn a doomed decoder
    /// every frame (P0.4).
    #[test]
    fn failed_live_lane_is_reaped_with_cooldown() {
        let dir = std::env::temp_dir().join("sb_app_failed_live_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.mp4");
        std::fs::write(&bad, b"this is not a video file").unwrap();

        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::Normal), // live lanes enabled
            inputs: vec![bad.clone()],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        assert!(pump_until(&mut app, |a| a.clips.len() == 1), "ingest");

        // Spawning on garbage succeeds — the failure surfaces async via
        // `failed()` — so install the lane the way update_live would.
        let player = sb_media::SeekablePlayer::spawn(&bad, 64, 36, 0.0, None)
            .expect("spawn returns a handle; open failure is async");
        app.live_sel = Some(SelLive {
            clip: 0,
            path: bad.clone(),
            player,
            spawned: Instant::now(),
            first_frame: None,
            duration: None,
        });
        assert!(
            pump_until(&mut app, |a| a.live_sel.is_none()),
            "failed lane never reaped"
        );
        assert!(
            app.live_retry.contains_key(&bad),
            "failure recorded for cooldown"
        );

        // Even with a plausible thumbnail (spawn preconditions met), the
        // cooldown blocks a respawn — no rapid spawn-fail loop.
        app.clips[0].thumb = Thumb::Ready {
            slot: 0,
            at: Instant::now(),
            tw: 64,
            th: 36,
        };
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        for _ in 0..50 {
            let _ = app.frame(0.016, vp);
        }
        assert!(app.live_sel.is_none(), "cooldown must block respawn");
    }

    /// Panning the selected tile offscreen parks its decoder: frames stop
    /// draining (bounded backpressure stalls the reader), hires uploads
    /// stop, `animating` releases the loop — and scrolling back resumes
    /// the SAME stream, no respawn (P0.4). Needs ffmpeg.
    #[test]
    fn offscreen_selection_parks_and_resumes_without_respawn() {
        let have_ffmpeg = std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !have_ffmpeg {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_app_park_test");
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("park.mp4");
        if !clip.exists() {
            let ok = std::process::Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=8:size=320x180:rate=30")
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

        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::Normal),
            inputs: vec![clip],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        assert!(
            pump_until(&mut app, |a| a
                .live_sel
                .as_ref()
                .is_some_and(|l| l.first_frame.is_some())),
            "selected live stream never started"
        );
        let spawned0 = app.live_sel.as_ref().unwrap().spawned;

        // Pad the library so row 0 can actually leave the viewport, then
        // trackpad-pan away (selection unchanged — keyboard moves always
        // scroll to the selection, so only panning parks).
        let now = Instant::now();
        for i in 0..600 {
            app.clips.push(Clip {
                path: PathBuf::from(format!("pad/{i}.mp4")),
                readable: false,
                cloud: false,
                cached: false,
                spawned: now,
                scale: 1.0,
                emph: 0.0,
                thumb: Thumb::Failed,
                anim: Thumb::Failed,
            });
        }
        app.scroll = 1e6;
        app.scroll_target = 1e6;
        assert!(
            pump_until(&mut app, |a| a.sel_parked),
            "offscreen selection never parked"
        );

        // Parked: the stream survives, nothing uploads…
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        for _ in 0..30 {
            let f = app.frame(0.016, vp);
            assert!(f.hires_upload.is_none(), "parked stream must not upload");
        }
        assert!(app.live_sel.is_some(), "parked stream must survive");
        // …and the loop is allowed to go idle despite the live lane.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut idle = false;
        while Instant::now() < deadline {
            if !app.frame(0.016, vp).animating {
                idle = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(idle, "parked live lane kept Frame.animating true");

        // Pan back: the same decoder resumes serving frames.
        app.scroll = 0.0;
        app.scroll_target = 0.0;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut resumed = false;
        while Instant::now() < deadline {
            if app.frame(0.016, vp).hires_upload.is_some() {
                resumed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(resumed, "no frames after unparking");
        assert_eq!(
            app.live_sel.as_ref().unwrap().spawned,
            spawned0,
            "resume must reuse the parked decoder, not respawn"
        );
    }

    /// Quickview seekbar: press grabs the bar (keyframe scrub), drag
    /// tracks the pointer, release lands an exact seek — all on the same
    /// resident decoder. Needs ffmpeg; skipped quietly when missing.
    #[test]
    fn seekbar_click_and_drag_scrubs_the_stream() {
        let have_ffmpeg = std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !have_ffmpeg {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_app_scrub_test");
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("scrub.mp4");
        if !clip.exists() {
            let ok = std::process::Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=8:size=320x180:rate=30")
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

        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::Normal),
            inputs: vec![clip],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        assert!(
            pump_until(&mut app, |a| a
                .live_sel
                .as_ref()
                .is_some_and(|l| l.first_frame.is_some())),
            "selected live stream never started"
        );
        app.quickview = true;
        app.quickview_at = Instant::now();
        let _ = app.frame(
            0.016,
            Viewport {
                width: 1280.0,
                height: 800.0,
            },
        );
        let (bx, bw, bot) = app.seekbar_line().expect("seekbar geometry");

        // Reveal: pointer motion over the video arms the bar.
        let (vx, vy, vw, vh) = app.quickview_video_rect().unwrap();
        app.event(InputEvent::CursorMoved {
            x: vx + vw * 0.5,
            y: vy + vh * 0.5,
        });
        assert!(
            app.seekbar_alpha() > 0.9,
            "motion over the video reveals the bar"
        );

        // Grab at 75%: keyframe scrub starts, position reports the target.
        app.event(InputEvent::MouseDown {
            x: bx + bw * 0.75,
            y: bot - 3.0,
        });
        assert!(app.scrubbing, "press on the bar starts a scrub");
        let p = app.live_sel.as_ref().unwrap().position();
        assert!(
            (5.4..=6.6).contains(&p),
            "position tracks the grab point, got {p}"
        );
        // One frame with the bar revealed + hot exercises the draw path.
        let _ = app.frame(
            0.016,
            Viewport {
                width: 1280.0,
                height: 800.0,
            },
        );

        // Drag to 25%, release: exact landing near 2s on the SAME stream.
        app.event(InputEvent::CursorMoved {
            x: bx + bw * 0.25,
            y: bot - 3.0,
        });
        app.event(InputEvent::MouseUp {
            x: bx + bw * 0.25,
            y: bot - 3.0,
        });
        assert!(!app.scrubbing, "release ends the scrub");
        let l = app.live_sel.as_ref().unwrap();
        assert!(
            l.first_frame.is_some(),
            "scrubbing never respawns the decoder"
        );
        let p = l.position();
        assert!((1.4..=2.6).contains(&p), "release lands near 25%, got {p}");
        assert!(
            pump_until(&mut app, |a| a
                .live_sel
                .as_ref()
                .is_some_and(|l| l.player.buffered() > 0)),
            "no frames after the scrub landing"
        );
    }

    /// Wheel/trackpad over quickview scrubs the filmstrip: the strip
    /// slides with the gesture and the selection commits to the nearest
    /// chip, clamped at the ends.
    #[test]
    fn filmstrip_scroll_commits_selection() {
        let dir = std::env::temp_dir().join("sb_app_strip_scroll_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["a.mp4", "b.mp4", "c.mp4"] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None), // no live decoders in tests
            inputs: vec![dir.clone()],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        assert!(pump_until(&mut app, |a| a.clips.len() == 3), "ingest");
        app.quickview = true;
        app.quickview_at = Instant::now();
        app.strip_pos = 0.0;

        let (cw, _, _) = app.strip_geom();
        let step = cw + app.tuning.strip_gap;
        // One chip-step of leftward gesture advances the selection…
        app.event(InputEvent::Scroll { dx: -step, dy: 0.0 });
        assert_eq!(app.selected, 1, "one chip-step advances the selection");
        // …and a huge fling clamps at the last clip.
        app.event(InputEvent::Scroll {
            dx: -step * 50.0,
            dy: 0.0,
        });
        assert_eq!(app.selected, 2, "the strip clamps at the ends");
        // Grid pan must not have moved under the modal.
        assert_eq!(app.scroll_target, 0.0, "the backdrop grid never pans");
    }

    /// Shuffle reorders the clip vector and remaps EVERY index-keyed
    /// reference through the permutation: the path→index map, atlas slot
    /// owners, and the selection (which must stay on the same clip).
    #[test]
    fn shuffle_remaps_every_index_keyed_reference() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        // Demo mode skips the index map (no media routing); build it the
        // way real ingest does so the remap invariant is checkable.
        for (i, c) in app.clips.iter().enumerate() {
            app.index.insert(c.path.clone(), i);
        }
        app.selected = 5;
        let sel_path = app.clips[5].path.clone();
        // A resident thumb: clip 3 owns slot 0.
        app.clips[3].thumb = Thumb::Ready {
            slot: 0,
            at: Instant::now(),
            tw: 64,
            th: 36,
        };
        app.slots[0] = Some((3, SlotKind::Static));
        // A few unswept clips: they must sink to the tail, in order.
        let uncached: Vec<PathBuf> = [7usize, 11, 40]
            .iter()
            .map(|&i| {
                app.clips[i].cached = false;
                app.clips[i].path.clone()
            })
            .collect();

        app.shuffle_library();

        assert_eq!(
            app.clips[app.selected].path, sel_path,
            "selection follows its clip"
        );
        let n = app.clips.len();
        let tail: Vec<PathBuf> = app.clips[n - 3..].iter().map(|c| c.path.clone()).collect();
        assert_eq!(
            tail, uncached,
            "uncached clips end up last, original order preserved"
        );
        for (i, c) in app.clips.iter().enumerate() {
            assert_eq!(
                app.index.get(&c.path),
                Some(&i),
                "path→index stays consistent after the permutation"
            );
        }
        let (owner, _) = app.slots[0].expect("slot survives the shuffle");
        assert!(
            matches!(app.clips[owner].thumb, Thumb::Ready { slot: 0, .. }),
            "the slot's owner pointer moved with its clip"
        );
    }

    /// The skip timer advances the selection once the current clip's time
    /// is up, wraps at the end of the library, and holds while off.
    #[test]
    fn skip_timer_advances_and_wraps() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None), // no live: counts from selection
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let expired = Instant::now() - Duration::from_secs(60);
        app.tuning.skip_timer_s = 5.0;

        // Off: an expired countdown does nothing.
        app.sel_changed_at = expired;
        let _ = app.frame(0.016, vp);
        assert_eq!(app.selected, 0, "timer off: selection holds");

        // On: it advances…
        app.skip_timer_since = Some(expired);
        let _ = app.frame(0.016, vp);
        assert_eq!(app.selected, 1, "timer on: selection advances");
        // …and re-arms (the new clip gets its own countdown).
        let _ = app.frame(0.016, vp);
        assert_eq!(app.selected, 1, "fresh selection: countdown restarts");

        // Wraps from the last clip back to the first.
        app.selected = app.clips.len() - 1;
        app.sel_changed_at = expired;
        let _ = app.frame(0.016, vp);
        assert_eq!(app.selected, 0, "wraps to the first clip");
    }

    /// The fullview chapter bar: synthesized checkpoint counts follow
    /// the duration ladder, chip hit-tests agree with the strip
    /// geometry, Esc peels the bar before exiting fullview, and leaving
    /// fullview or the bar's clip drops the state.
    #[test]
    fn chapter_bar_checkpoints_and_lifecycle() {
        // The checkpoint ladder for chapterless clips.
        assert_eq!(checkpoint_count(30.0), 0, "under a minute: no bar");
        assert_eq!(checkpoint_count(60.0), 4, "a minute up: every 25%");
        assert_eq!(checkpoint_count(200.0), 8, "over three minutes: 8");
        assert_eq!(checkpoint_count(700.0), 10, "over ten minutes: 10");

        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let open = |app: &mut Switchblade, n: usize| {
            app.fullview = true;
            let last = n.saturating_sub(1) as f32;
            app.chapters = Some(ChapterBar {
                path: app.clips[app.selected].path.clone(),
                duration: Some(400.0),
                times: Some((0..n).map(|k| k as f64 * 50.0).collect()),
                open: true,
                slide: 1.0,
                pos: last * 0.5,
                target: last * 0.5,
            });
        };

        // Chip centers map back to their index (8 chips all fit at
        // 1280×800 with pos centered), the gap between chips is dead.
        open(&mut app, 8);
        let (cw, ch, sy) = app.strip_geom();
        let step = cw + app.tuning.strip_gap;
        let center = app.chapters.as_ref().unwrap().pos;
        for i in 0..8 {
            let cx = vp.width * 0.5 + (i as f32 - center) * step;
            assert_eq!(
                app.chapter_chip_at(cx, sy + ch * 0.5),
                Some(i),
                "chip {i} center hit"
            );
            assert_eq!(
                app.chapter_chip_at(cx + step * 0.5, sy + ch * 0.5),
                None,
                "gap right of chip {i} is dead space"
            );
        }

        // Esc slides the bar down but stays in fullview; the state
        // drops once the (snap-mode) slide lands in step().
        app.event(InputEvent::Key {
            key: Key::Escape,
            repeat: false,
        });
        assert!(
            app.chapters.as_ref().is_some_and(|b| !b.open),
            "Esc closes the bar first"
        );
        assert!(app.fullview, "fullview survives the first Esc");
        for _ in 0..3 {
            let _ = app.frame(0.016, vp);
        }
        assert!(app.chapters.is_none(), "landed slide drops the state");
        app.event(InputEvent::Key {
            key: Key::Escape,
            repeat: false,
        });
        assert!(!app.fullview, "second Esc exits fullview");

        // Exiting fullview drops the bar outright…
        open(&mut app, 8);
        app.fullview = false;
        let _ = app.frame(0.016, vp);
        assert!(app.chapters.is_none(), "bar dies with fullview");

        // …and so does the selection leaving the bar's clip.
        open(&mut app, 8);
        app.selected += 1;
        let _ = app.frame(0.016, vp);
        assert!(
            app.chapters.is_none(),
            "bar drops when the selection moves off its clip"
        );
    }

    /// `--no-config` runs on the internal defaults with no config file
    /// watched at all — behavior can't be steered by a stray
    /// ./switchblade.toml or ~/.config file (every test here passes it).
    #[test]
    fn no_config_runs_on_internal_defaults() {
        let app = Switchblade::with_options(Options {
            demo: true,
            no_config: true,
            ..Options::default()
        });
        assert!(app.tuning_file.is_none(), "no config file is watched");
        assert_eq!(
            app.tuning.selection_scale,
            Tuning::default().selection_scale,
            "internal defaults apply"
        );
    }

    /// Modal views freeze the world behind them: the grid backdrop is a
    /// static snapshot from the moment the modal opened — no per-clip
    /// springs run, selection changes don't animate hidden tiles, and
    /// the snapshot drops (simulation resumes) the moment the modal
    /// closes.
    #[test]
    fn modal_backdrop_freezes_the_grid() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None), // springs snap in one frame
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        // Pin the scale the test observes — a user-level config on the
        // machine running the tests could otherwise neutralize it.
        app.tuning.selection_scale = 1.3;
        app.tuning.selection_zoom_boost = 0.0;
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let _ = app.frame(0.016, vp);
        assert!(
            app.clips[0].scale > 1.05,
            "selected tile scaled up in the open grid"
        );

        app.quickview = true;
        let _ = app.frame(0.016, vp);
        assert!(app.frozen_grid.is_some(), "backdrop snapshot captured");

        // Selection moves while the modal is up: the hidden grid must
        // not simulate — no spring catches up, the snapshot stands.
        app.selected = 1;
        for _ in 0..3 {
            let _ = app.frame(0.016, vp);
        }
        assert!(
            app.clips[1].scale < 1.01,
            "hidden tile never animated behind the modal"
        );
        assert!(
            app.clips[0].scale > 1.05,
            "old selection never shrank behind the modal"
        );

        // Exit: snapshot drops, simulation resumes (snap mode = 1 frame).
        app.quickview = false;
        let _ = app.frame(0.016, vp);
        assert!(app.frozen_grid.is_none(), "snapshot dropped on exit");
        assert!(
            app.clips[1].scale > 1.05 && app.clips[0].scale < 1.01,
            "springs resumed once the grid is visible again"
        );
    }

    /// Fullview must never hover-play the hidden grid: the pointer still
    /// sits "over" an invisible tile behind the black backdrop, and each
    /// pointer rest used to cold-spawn a hover decoder for a clip nobody
    /// can see — stealing CPU and the media engine from the video being
    /// watched.
    #[test]
    fn fullview_suppresses_hidden_grid_hover() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let g = app.tuning.gap;
        app.event(InputEvent::CursorMoved {
            x: g + 10.0,
            y: g + 10.0,
        });
        let _ = app.frame(0.016, vp);
        assert_eq!(app.hovered, Some(0), "grid hover works normally");

        app.fullview = true;
        let _ = app.frame(0.016, vp);
        assert_eq!(app.hovered, None, "fullview: no hidden-tile hover");

        app.fullview = false;
        let _ = app.frame(0.016, vp);
        assert_eq!(app.hovered, Some(0), "hover returns with the grid");
    }

    /// Fullview's chapter-probe prewarm defers to the video being
    /// watched: nothing fires while a live stream is expected but hasn't
    /// delivered its first frame — until the dead-stream grace expires.
    #[test]
    fn fullview_prewarm_waits_for_the_playing_stream() {
        let dir = std::env::temp_dir().join("sb_app_prewarm_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.mp4"), b"").unwrap();
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::Normal), // live video expected
            inputs: vec![dir.join("a.mp4")],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        assert!(pump_until(&mut app, |a| a.clips.len() == 1), "ingest");
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };

        // Stream expected (level normal) but not up: the prewarm holds.
        app.sel_changed_at = Instant::now();
        app.fullview = true;
        for _ in 0..3 {
            let _ = app.frame(0.016, vp);
        }
        assert!(
            app.chapter_probe.is_empty(),
            "prewarm must wait for the watched stream's first frame"
        );

        // A stream that never delivers releases the gate after the grace
        // — the storyboard must not starve behind a dead decoder.
        app.sel_changed_at = Instant::now() - Duration::from_secs_f32(PREWARM_GRACE_S + 1.0);
        let _ = app.frame(0.016, vp);
        assert!(
            app.chapter_probe.contains_key(&app.clips[0].path),
            "grace expiry lets the probe prewarm through"
        );
    }

    /// Strip chips keep a fixed height and take each clip's true aspect
    /// for width: a portrait clip's chip is narrower (never taller),
    /// neighbors space by the sum of half-widths, hit-tests follow the
    /// variable layout, and the chapter bar adopts the selected clip's
    /// shape for all its chips.
    #[test]
    fn strip_chips_keep_true_aspect_at_fixed_height() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        let (_, ch, sy) = app.strip_geom();
        let gap = app.tuning.strip_gap;
        // Clip 5 is portrait (9:16); its neighbors have no thumbs and
        // default to 16:9.
        app.clips[5].thumb = Thumb::Ready {
            slot: 0,
            at: Instant::now(),
            tw: 360,
            th: 640,
        };
        let layout = app.strip_layout(5.0);
        let find = |i: usize| {
            layout
                .iter()
                .find(|&&(k, _, _)| k == i)
                .copied()
                .unwrap_or_else(|| panic!("chip {i} missing from layout"))
        };
        let (_, cx5, w5) = find(5);
        let (_, cx6, w6) = find(6);
        let (_, cx4, w4) = find(4);
        assert!(
            (w5 - ch * 9.0 / 16.0).abs() < 0.5,
            "portrait chip is 9:16 at the fixed height, got {w5}"
        );
        assert!(
            (w6 - ch * 16.0 / 9.0).abs() < 0.5,
            "thumbless neighbor stays 16:9, got {w6}"
        );
        assert!(
            (cx6 - cx5 - ((w5 + w6) * 0.5 + gap)).abs() < 0.5,
            "neighbors space by half-widths plus the gap"
        );
        assert!(
            ((cx5 - cx4) - ((w4 + w5) * 0.5 + gap)).abs() < 0.5,
            "left neighbor spaces the same way"
        );
        // Hit-tests follow the variable widths.
        app.quickview = true;
        app.strip_pos = 5.0;
        assert_eq!(app.strip_chip_at(cx5, sy + ch * 0.5), Some(5));
        assert_eq!(app.strip_chip_at(cx6, sy + ch * 0.5), Some(6));
        assert_eq!(
            app.strip_chip_at(cx5 + w5 * 0.5 + gap * 0.5, sy + ch * 0.5),
            None,
            "the gap between chips stays dead space"
        );
        // The chapter bar adopts the selected clip's aspect.
        app.selected = 5;
        assert!(
            (app.chapter_chip_w() - ch * 9.0 / 16.0).abs() < 0.5,
            "chapter chips share the portrait clip's shape"
        );
        app.selected = 6;
        assert!(
            (app.chapter_chip_w() - ch * 16.0 / 9.0).abs() < 0.5,
            "…and a 16:9 clip's shape"
        );
    }

    /// End to end: `g` enters fullview and raises the bar, the probe
    /// reports the file's real chapters, and clicking a chip seeks the
    /// resident stream to that chapter and sends the bar down. Needs
    /// ffmpeg; skipped quietly when missing.
    #[test]
    fn chapter_click_seeks_the_stream() {
        let have_ffmpeg = std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !have_ffmpeg {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_app_chapter_click_test");
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("plain.mp4");
        let clip = dir.join("chaptered.mp4");
        if !clip.exists() {
            let ok = std::process::Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=8:size=320x180:rate=30")
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
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to generate test clip");
            let metafile = dir.join("chapters.txt");
            std::fs::write(
                &metafile,
                ";FFMETADATA1\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=3000\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=3000\nEND=6000\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=6000\nEND=8000\n",
            )
            .unwrap();
            let ok = std::process::Command::new("ffmpeg")
                .args(["-y", "-v", "error"])
                .arg("-i")
                .arg(&plain)
                .arg("-i")
                .arg(&metafile)
                .args(["-map_metadata", "1", "-codec", "copy"])
                .arg(&clip)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to mux chapters");
        }

        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::Normal),
            inputs: vec![clip],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        assert!(
            pump_until(&mut app, |a| a
                .live_sel
                .as_ref()
                .is_some_and(|l| l.first_frame.is_some())),
            "selected live stream never started"
        );

        app.toggle_chapters();
        assert!(app.fullview, "g enters fullview");
        assert!(
            app.chapters.as_ref().is_some_and(|b| b.open),
            "the bar opens"
        );
        assert!(
            pump_until(&mut app, |a| a
                .chapters
                .as_ref()
                .is_some_and(|b| b.times.is_some() && b.slide > 0.9)),
            "chapter probe never answered / bar never slid up"
        );
        let bar = app.chapters.as_ref().unwrap().clone();
        let times = bar.times.as_ref().unwrap();
        assert_eq!(times.len(), 3, "the file's real chapters, not checkpoints");
        assert!((times[1] - 3.0).abs() < 0.05, "second chapter at 3s");

        // Click the third chip: the SAME stream lands on ~6s and the
        // bar slides back down.
        let (cw, ch, sy) = app.strip_geom();
        let step = cw + app.tuning.strip_gap;
        let cx = 1280.0 * 0.5 + (2.0 - bar.pos) * step;
        let cy = sy + (1.0 - bar.slide) * (ch + 44.0) + ch * 0.5;
        app.event(InputEvent::MouseDown { x: cx, y: cy });
        let l = app.live_sel.as_ref().expect("stream survives the click");
        assert!(l.first_frame.is_some(), "no respawn");
        let p = l.position();
        assert!((5.5..=6.5).contains(&p), "seeked to chapter 3, got {p}");
        assert!(
            app.chapters.as_ref().is_none_or(|b| !b.open),
            "the bar slides down after the click"
        );
        assert!(app.fullview, "fullview stays");
    }

    /// `jump_random` always lands somewhere else, and only ever on a clip
    /// whose thumbnail is already cached (no on-demand gen explosions).
    #[test]
    fn jump_random_lands_only_on_cached_clips() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        for _ in 0..50 {
            let before = app.selected;
            app.jump_random();
            assert_ne!(app.selected, before, "random jump moved the selection");
        }
        // Only two cached clips: every jump lands on one of them.
        for c in &mut app.clips {
            c.cached = false;
        }
        app.clips[10].cached = true;
        app.clips[20].cached = true;
        for _ in 0..20 {
            app.jump_random();
            assert!(
                [10, 20].contains(&app.selected),
                "jump landed on an uncached clip: {}",
                app.selected
            );
        }
        // No cached candidates at all: the jump is a no-op.
        for c in &mut app.clips {
            c.cached = false;
        }
        let before = app.selected;
        app.jump_random();
        assert_eq!(app.selected, before, "nothing cached: selection holds");
    }
}
