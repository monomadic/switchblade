//! sb-app: application state and grid logic, headless of any OS/GPU types.
//! Implements the `sb_window::App` trait (PLAN.md §12).

mod commands;
mod ingest;
mod tuning;

pub use tuning::Tuning;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use sb_media::{MediaService, Recipe, ThumbResult};
use sb_window::{
    App, AtlasCfg, Blur, Frame, HiresFrame, InputEvent, Key, ThumbUpload, Tile, Viewport,
    WindowCommand,
};

use commands::{Action, KeyMap};
use tuning::{alpha, TuningFile};

/// Rows beyond the viewport to prefetch thumbnails for.
const PREFETCH_ROWS: usize = 2;
/// Thumbnail crossfade duration once pixels arrive.
const THUMB_FADE_S: f32 = 0.3;

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

/// The hovered tile's video playback: tile-sized, into an atlas slot.
struct LiveState {
    clip: usize,
    player: sb_media::LivePlayer,
    slot: usize,
    /// Set when the first frame arrives; the tile switches to video then.
    first_frame: Option<Instant>,
}

/// The selected clip's live stream: decoded once at quickview resolution
/// into the mipmapped hires texture. The tile shows it downscaled and the
/// quickview modal shows it big — one decoder, one timeline, no handoffs.
struct SelLive {
    clip: usize,
    player: sb_media::LivePlayer,
    spawned: Instant,
    first_frame: Option<Instant>,
}

struct Clip {
    path: PathBuf,
    readable: bool,
    /// iCloud placeholder — shown with a cloud badge, never read (reading
    /// would trigger a download).
    cloud: bool,
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
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub anim: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { anim: true }
    }
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
    /// unwatched player's bounded frame queue fills and stalls ffmpeg
    /// after a few frames, so warmth is all but free.
    warm: Vec<SelLive>,
    /// The newest hires frame this tick, routed to Frame.hires_upload.
    hires_frame: Option<HiresFrame>,
    sel_changed_at: Instant,
    hover_changed_at: Instant,
    demo: bool,
    /// Runtime animation toggle (`a` key / --no-anim), ANDed with tuning.anim.
    anim_on: bool,
    /// Window focus state + the runtime toggle for pause-when-unfocused.
    focused: bool,
    focus_pause_on: bool,
    anim_grid: u32,
    atlas_cfg: AtlasCfg,
    /// Internal fullscreen-ish preview of the selected clip.
    quickview: bool,
    quickview_at: Instant,
    /// Filmstrip slide position (in clip-index units), springing toward
    /// the selected index with the keyboard chase curve.
    strip_pos: f32,
    /// Background job counters for the progress indicator.
    jobs_total: u64,
    jobs_done: u64,
    jobs_finished_at: Option<Instant>,
    tuning: Tuning,
    keymap: KeyMap,
    tuning_file: TuningFile,
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
    last_scroll_event: Instant,
    viewport: Viewport,
    cmds: Vec<WindowCommand>,
    title: String,
}

impl Default for Switchblade {
    fn default() -> Self {
        Self::new()
    }
}

impl Switchblade {
    pub fn new() -> Self {
        Self::with_options(Options::default())
    }

    pub fn with_options(opts: Options) -> Self {
        let rx = ingest::spawn_stdin_reader();
        let demo = rx.is_none();

        // Load config up front: atlas geometry and the media recipe are
        // startup-only (the rest keeps hot-reloading per frame).
        let mut tuning_file = TuningFile::new(PathBuf::from("switchblade.toml"));
        let (tuning, keymap) = match tuning_file.poll() {
            Some(cfg) => (cfg.tuning, cfg.keymap),
            None => (Tuning::default(), KeyMap::default()),
        };
        let (slot_w, slot_h) = (
            tuning.thumb_width.clamp(64, 2048),
            tuning.thumb_height.clamp(36, 2048),
        );
        let atlas_cfg = AtlasCfg {
            slot_w,
            slot_h,
            cols: (tuning.atlas_width.min(8192) / slot_w).max(1),
            rows: (tuning.atlas_height.min(8192) / slot_h).max(1),
            hires_w: tuning.quickview_max_width.clamp(320, 4096),
            hires_h: tuning.quickview_max_height.clamp(180, 4096),
        };
        log::info!(
            "atlas: {}x{} slots of {slot_w}x{slot_h} ({} MB)",
            atlas_cfg.cols,
            atlas_cfg.rows,
            atlas_cfg.tex_w() as u64 * atlas_cfg.tex_h() as u64 * 4 / (1024 * 1024)
        );
        let anim_grid = tuning.anim_grid.clamp(1, 4);
        let recipe = Recipe {
            thumb_w: slot_w,
            thumb_h: slot_h,
            quality: tuning.thumb_quality,
            anim_grid,
        };

        let mut app = Self {
            clips: Vec::new(),
            index: HashMap::new(),
            rx,
            media: MediaService::new(recipe),
            slots: vec![None; atlas_cfg.slots()],
            live_sel: None,
            live_hover: None,
            warm: Vec::new(),
            hires_frame: None,
            sel_changed_at: Instant::now(),
            hover_changed_at: Instant::now(),
            demo,
            anim_on: opts.anim,
            focused: true,
            focus_pause_on: true,
            anim_grid,
            atlas_cfg,
            quickview: false,
            quickview_at: Instant::now(),
            strip_pos: 0.0,
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
            last_cols: 0,
            last_tiles: Vec::new(),
            transition: None,
            t0: Instant::now(),
            motion: true,
            anim_rendered: false,
            wake_until: Instant::now() + Duration::from_secs(1),
            last_scroll_event: Instant::now(),
            viewport: Viewport {
                width: 1280.0,
                height: 800.0,
            },
            cmds: Vec::new(),
            title: String::new(),
        };
        if demo {
            log::info!("stdin is a tty — demo mode with {DEMO_TILES} fake tiles");
            let now = Instant::now();
            for i in 0..DEMO_TILES {
                app.clips.push(Clip {
                    path: PathBuf::from(format!("demo/clip_{i:04}.mp4")),
                    readable: true,
                    cloud: false,
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

    // --- input ---

    fn key(&mut self, key: Key) {
        // Movement keys are reserved; everything else goes through the keymap.
        match key {
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
            Action::ToggleFullscreen => self.cmds.push(WindowCommand::ToggleFullscreen),
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
                }
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

    /// True while animation/live playback should rest because the window
    /// lost focus (default on; `p` toggles, `pause_unfocused` configures).
    fn paused(&self) -> bool {
        !self.focused && self.tuning.pause_unfocused && self.focus_pause_on
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
        loop {
            match rx.try_recv() {
                Ok(item) => {
                    // spawn fade-in (field update: `rx` is borrowed here)
                    self.wake_until = self
                        .wake_until
                        .max(Instant::now() + Duration::from_millis(600));
                    self.index.insert(item.path.clone(), self.clips.len());
                    // Cloud placeholders never request a thumbnail: reading
                    // the file would force iCloud to download it.
                    let thumb = if item.cloud {
                        Thumb::Failed
                    } else {
                        Thumb::None
                    };
                    self.clips.push(Clip {
                        path: item.path,
                        readable: item.readable,
                        cloud: item.cloud,
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
                    break;
                }
            }
        }
    }

    fn step(&mut self, dt: f32) {
        let t = self.tuning.clone();

        // Zoom spring, anchored so the content at the viewport center stays
        // put while tile size (and column count) reflows around it.
        let old_zoom = self.zoom;
        self.zoom += (self.zoom_target - self.zoom) * alpha(t.zoom_smoothing, dt);
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
        self.scroll += (self.scroll_target - self.scroll) * alpha(self.chase, dt);

        let hover_now = self.tile_at(&lay, self.cursor.0, self.cursor.1);
        if hover_now != self.hovered {
            self.hovered = hover_now;
            self.hover_changed_at = Instant::now();
        }

        // Selection stands out more the further you zoom out.
        let boost = (1.0 / self.zoom.max(0.05))
            .powf(t.selection_zoom_boost)
            .clamp(1.0, 2.2);
        let sel_scale = t.selection_scale * boost;

        // Tile scale + emphasis springs (selected > hover > rest); both
        // keyboard moves and hover ride the same tween. Track whether any
        // spring is still in flight for idle throttling.
        let mut motion = (self.scroll_target - self.scroll).abs() > 0.3
            || (self.zoom_target - self.zoom).abs() > 1e-3;
        let a = alpha(t.scale_smoothing, dt);
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
        self.motion = motion;

        // Quickview filmstrip slides with the same curve as keyboard moves.
        if self.quickview {
            let target = self.selected as f32;
            self.strip_pos += (target - self.strip_pos) * alpha(t.strip_snap_strength, dt);
            if (target - self.strip_pos).abs() > 0.001 {
                self.motion = true;
            }
        }
    }

    /// Filmstrip geometry: chip width/height and the strip's top y.
    fn strip_geom(&self) -> (f32, f32, f32) {
        let ch = self.tuning.strip_height.max(24.0);
        let cw = ch * 16.0 / 9.0;
        (cw, ch, self.viewport.height - ch - 22.0)
    }

    /// Which filmstrip chip is under (x, y), if any.
    fn strip_chip_at(&self, x: f32, y: f32) -> Option<usize> {
        if !self.quickview || self.clips.is_empty() {
            return None;
        }
        let (cw, ch, sy) = self.strip_geom();
        if y < sy - 8.0 || y > sy + ch + 8.0 {
            return None;
        }
        let step = cw + self.tuning.strip_gap;
        let rel = (x - self.viewport.width * 0.5) / step + self.strip_pos;
        let i = rel.round();
        if i < 0.0 || i as usize >= self.clips.len() {
            return None;
        }
        ((rel - i).abs() * step <= cw * 0.5).then_some(i as usize)
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

        if !(self.tuning.anim && self.anim_on) || lay.tile_w < self.tuning.anim_min_tile_w {
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

    /// Collect finished thumbnails into atlas uploads for this frame.
    fn drain_media(&mut self, lay: &Layout) -> Vec<ThumbUpload> {
        let mut uploads = Vec::new();
        while let Some(result) = self.media.try_recv() {
            self.jobs_done += 1;
            self.wake(0.6); // thumb crossfade + progress bar linger
            match result {
                ThumbResult::Ready { path, w, h, rgba } => {
                    let Some(&i) = self.index.get(&path) else {
                        continue;
                    };
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
            }
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
        let base = !self.demo && !self.paused();
        let lanes = base && self.tuning.live_preview;
        let delay_ms = self.tuning.live_delay_ms;
        // One selected stream feeds both the tile and the quickview modal.
        // It runs while browsing (live_preview) and always during quickview
        // — quickview is an explicit "play this".
        let sel_target = (lanes || (base && self.quickview)).then_some(self.selected);
        // The hover lane is suppressed during quickview (grid is hidden)
        // and when hover sits on the selected tile (that stream owns it).
        let hover_target = if lanes && !self.quickview {
            self.hovered.filter(|h| *h != self.selected)
        } else {
            None
        };

        // Pre-warm decoders for the four movement destinations (±1 and
        // ±row) so a selection move shows video instantly instead of
        // paying a cold ffmpeg spawn (~1s on 4K sources). Active in
        // quickview and — when live_preview is on — in the grid.
        let mut warm_targets: Vec<usize> = Vec::new();
        if base && (self.quickview || lanes) {
            let s = self.selected;
            let cols = lay.cols.max(1);
            let n = self.clips.len();
            let mut push = |i: usize| {
                if i < n && i != s && !warm_targets.contains(&i) {
                    warm_targets.push(i);
                }
            };
            if s > 0 {
                push(s - 1);
            }
            push(s + 1);
            if s >= cols {
                push(s - cols);
            }
            push(s + cols);
        }

        // Stop lanes whose target moved away. In quickview the selected
        // stream demotes to a warm neighbor instead of dying — reversing
        // direction picks it right back up, still on its timeline.
        if self
            .live_sel
            .as_ref()
            .is_some_and(|l| sel_target != Some(l.clip))
        {
            let l = self.live_sel.take().unwrap();
            if warm_targets.contains(&l.clip) {
                self.warm.push(l);
            }
        }
        if let Some(l) = &self.live_hover {
            if hover_target != Some(l.clip) {
                let slot = l.slot;
                self.live_hover = None;
                self.slots[slot] = None;
            }
        }

        // Start lanes whose target has settled. Quickview skips the settle
        // delay — the user's explicit action goes to the forefront — and
        // promotes a pre-warmed neighbor when one is ready: its queue
        // already holds due frames, so the video shows this same tick.
        // Promotion MUST happen before the warm pool is pruned: the pool
        // is keyed by the new selection's neighbors, which never include
        // the selection itself — pruning first would kill the very decoder
        // that was warmed for this moment (the bug that made every advance
        // pay full ffmpeg spawn latency).
        if self.live_sel.is_none() {
            if let Some(i) = sel_target {
                if let Some(pos) = self.warm.iter().position(|w| w.clip == i) {
                    let l = self.warm.remove(pos);
                    log::debug!(
                        "promoted warm decoder for clip {i} ({} frames buffered)",
                        l.player.buffered()
                    );
                    self.live_sel = Some(l);
                } else if self.quickview
                    || self.sel_changed_at.elapsed().as_millis() as f32 >= delay_ms
                {
                    self.live_sel = self.start_sel_live(i);
                }
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
        if sel_ready && self.sel_changed_at.elapsed().as_millis() as f32 >= delay_ms {
            for &i in &warm_targets {
                if self.warm.iter().all(|w| w.clip != i) {
                    if let Some(l) = self.start_sel_live(i) {
                        self.warm.push(l);
                    }
                }
            }
        }
        if self.live_hover.is_none() {
            if let Some(i) = hover_target {
                if self.hover_changed_at.elapsed().as_millis() as f32 >= delay_ms {
                    self.live_hover = self.start_live(lay, i);
                }
            }
        }

        if let Some(live) = &mut self.live_hover {
            if let Some(rgba) = live.player.take_frame() {
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
        }
        if let Some(live) = &mut self.live_sel {
            if let Some(rgba) = live.player.take_frame() {
                if live.first_frame.is_none() {
                    live.first_frame = Some(Instant::now());
                    log::debug!(
                        "sel live clip {} first frame {:.0}ms after spawn",
                        live.clip,
                        live.spawned.elapsed().as_secs_f32() * 1000.0
                    );
                }
                self.hires_frame = Some(HiresFrame {
                    w: live.player.w,
                    h: live.player.h,
                    rgba,
                });
            }
        }
    }

    /// The selected clip's decoder: natural resolution, capped at the hires
    /// texture (never upscaled past the source when its dims are known).
    fn start_sel_live(&mut self, i: usize) -> Option<SelLive> {
        let clip = self.clips.get(i)?;
        let (tw, th, path) = match clip.thumb {
            Thumb::Ready { tw, th, .. } if clip.readable && !clip.cloud => {
                (tw, th, clip.path.clone())
            }
            _ => return None,
        };
        let meta = sb_media::cached_meta(&path);
        let (sw, sh) = meta
            .as_ref()
            .and_then(|m| Some((m.width? as f32, m.height? as f32)))
            .unwrap_or((tw as f32 * 4.0, th as f32 * 4.0));
        let (bw, bh) = (self.atlas_cfg.hires_w as f32, self.atlas_cfg.hires_h as f32);
        let scale = (bw / sw).min(bh / sh).min(1.0);
        let (dw, dh) = (((sw * scale) as u32).max(2), ((sh * scale) as u32).max(2));
        // Start where the thumbnail was taken, so video continues from the
        // frame the tile already shows instead of jolting to 0:00.
        let seek = meta
            .as_ref()
            .and_then(|m| m.duration)
            .map(|d| (d * sb_media::SEEK_FRACTION).max(0.0))
            .unwrap_or(0.0);
        let fps = meta.as_ref().and_then(|m| m.fps).unwrap_or(30.0);
        let codec = meta.as_ref().and_then(|m| m.codec.as_deref());
        let player = sb_media::LivePlayer::spawn(&path, dw, dh, seek, fps, codec)?;
        log::debug!("selected live {dw}x{dh} @{seek:.1}s: {}", path.display());
        Some(SelLive {
            clip: i,
            player,
            spawned: Instant::now(),
            first_frame: None,
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
        let slot = self.alloc_slot(lay, SlotKind::Live)?;
        // Start where the thumbnail was taken, so video continues from
        // the frame the tile already shows instead of jolting to 0:00.
        let meta = sb_media::cached_meta(&path);
        let seek = meta
            .as_ref()
            .and_then(|m| m.duration)
            .map(|d| (d * sb_media::SEEK_FRACTION).max(0.0))
            .unwrap_or(0.0);
        let fps = meta.as_ref().and_then(|m| m.fps).unwrap_or(30.0);
        let codec = meta.as_ref().and_then(|m| m.codec.as_deref());
        let Some(player) = sb_media::LivePlayer::spawn(&path, tw, th, seek, fps, codec) else {
            log::debug!("live preview failed to start: {}", path.display());
            return None;
        };
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

        let now = Instant::now();
        let anim_t = now.saturating_duration_since(self.t0).as_secs_f32();
        self.anim_rendered = false;
        let mut tiles = Vec::new();
        // Z-order: grid tiles, then the hovered tile lifted above its
        // neighbors, then the selected tile on top of everything.
        let mut hovered_group: Vec<Tile> = Vec::new();
        let mut selected_group: Vec<Tile> = Vec::new();

        for row in first_row..=last_row {
            for col in 0..lay.cols {
                let i = row * lay.cols + col;
                if i >= self.clips.len() {
                    break;
                }
                let clip = &self.clips[i];

                // Spawn fade/scale-in.
                let fade = match now.checked_duration_since(clip.spawned) {
                    Some(el) => (el.as_secs_f32() * 1000.0 / t.fade_in_ms.max(1.0)).min(1.0),
                    None => 0.0,
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
                        let m = (now.saturating_duration_since(at).as_secs_f32() / THUMB_FADE_S)
                            .min(1.0);
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
                        if l.clip == i && l.first_frame.is_some() {
                            live_hires = Some((l.player.w as f32, l.player.h as f32));
                        }
                    }
                } else if hovered {
                    if let Some(live) = &self.live_hover {
                        if live.clip == i && live.first_frame.is_some() {
                            thumb = Some((live.slot, live.player.w as f32, live.player.h as f32));
                        }
                    }
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
                if e > 0.001 {
                    if let Some((_, tw, th)) = thumb {
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
                let anim_allowed = t.anim && self.anim_on && !emphasized && !self.paused();
                let anim = if anim_allowed {
                    match clip.anim {
                        Thumb::Ready { slot, at, tw, th } => Some((slot, at, tw as f32, th as f32)),
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
                    let (hw, hh) = (self.atlas_cfg.hires_w as f32, self.atlas_cfg.hires_h as f32);
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
                        let m = (now.saturating_duration_since(at).as_secs_f32() / THUMB_FADE_S)
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
                    // The selected tile always has a visible body, even
                    // before its thumbnail exists.
                    ([0.055, 0.055, 0.07], ease)
                } else {
                    ([0.0, 0.0, 0.0], ease * mix)
                };

                let (sb, hb, eb) = (t.selection_border, t.hover_border, t.empty_border);
                let (border_color, border_width, radius) = if selected {
                    (
                        [sb[0], sb[1], sb[2], ease],
                        t.border_width,
                        t.selection_corner_radius,
                    )
                } else if hovered {
                    ([hb[0], hb[1], hb[2], 0.35 * ease], 1.0, t.corner_radius)
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
            }
        }
        tiles.extend(hovered_group);
        tiles.extend(selected_group);

        // Photos-style reflow: when the column count changes (zoom/resize),
        // the previous layout fades out on top of the new one.
        if lay.cols != self.last_cols && !self.last_tiles.is_empty() {
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
        if self.quickview {
            if let Some(clip) = self.clips.get(self.selected) {
                let fade = (self.quickview_at.elapsed().as_secs_f32() / 0.15).min(1.0);
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
                // stands in for the brief first-frame window.
                let live = self
                    .live_sel
                    .as_ref()
                    .filter(|l| l.clip == self.selected && l.first_frame.is_some());
                let src = if let Some(l) = live {
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
                };
                // The video sits above the filmstrip.
                let (chip_w, chip_h, strip_y) = self.strip_geom();
                let avail_h = (strip_y - 18.0).max(60.0);
                if let Some((uv, tw, th, hires)) = src {
                    let a = tw / th.max(1.0);
                    let (mut w, mut h) = (vw * 0.88, avail_h * 0.92);
                    if a > w / h {
                        h = w / a;
                    } else {
                        w = h * a;
                    }
                    tiles.push(Tile {
                        x: (vw - w) * 0.5,
                        y: (avail_h - h) * 0.5 + 6.0,
                        w,
                        h,
                        color: [0.0, 0.0, 0.0, fade],
                        border_color: [0.0; 4],
                        corner_radius: t.selection_corner_radius,
                        border_width: 0.0,
                        uv,
                        uv2: [0.0; 4],
                        frame_fade: 0.0,
                        tex_mix: fade,
                        hires,
                    });
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
                // sliding on the keyboard chase spring. Foreground layer —
                // in quickview, actions live here; the grid is backdrop.
                let step = chip_w + t.strip_gap;
                let half = (vw / step) as i64 / 2 + 1;
                let center = self.strip_pos;
                for di in -half..=half {
                    let idx = center.round() as i64 + di;
                    if idx < 0 || idx as usize >= self.clips.len() {
                        continue;
                    }
                    let i = idx as usize;
                    let c = &self.clips[i];
                    let sel = i == self.selected;
                    let s = if sel { 1.10 } else { 1.0 };
                    let (w, h) = (chip_w * s, chip_h * s);
                    let cx = vw * 0.5 + (i as f32 - center) * step;
                    let (uv, has_tex) = match c.thumb {
                        Thumb::Ready { slot, tw, th, .. } => {
                            let (tw, th) = (tw as f32, th as f32);
                            let target_a = 16.0 / 9.0;
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
                        _ => ([0.0; 4], false),
                    };
                    let sb = t.selection_border;
                    let (border_color, border_width) = if sel {
                        ([sb[0], sb[1], sb[2], fade], t.border_width * 0.7)
                    } else {
                        ([0.30, 0.30, 0.34, 0.55 * fade], 1.0)
                    };
                    tiles.push(Tile {
                        x: cx - w * 0.5,
                        y: strip_y + (chip_h - h) * 0.5,
                        w,
                        h,
                        color: [0.03, 0.03, 0.04, fade],
                        border_color,
                        corner_radius: t.corner_radius,
                        border_width,
                        uv,
                        uv2: [0.0; 4],
                        frame_fade: 0.0,
                        tex_mix: if has_tex { fade } else { 0.0 },
                        hires: false,
                    });
                }
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
                let (bw, bh) = (110.0, 3.0);
                let bx = self.viewport.width - bw - 14.0;
                let by = self.viewport.height - bh - 12.0;
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
                tiles.push(bar(bx, bw, 0.10)); // track
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
            InputEvent::Scroll { dy, .. } => {
                let d = -dy * self.tuning.pan_sensitivity;
                self.scroll_target += d;
                self.scroll_vel = self.scroll_vel * 0.7 + d * 60.0 * 0.3;
                self.last_scroll_event = Instant::now();
                self.chase = self.tuning.snap_strength;
            }
            InputEvent::Pinch { delta } => {
                let factor = 1.0 + delta * self.tuning.pinch_sensitivity;
                self.set_zoom(self.zoom_target * factor.max(0.01));
            }
            InputEvent::CursorMoved { x, y } => {
                self.cursor = (x, y);
            }
            InputEvent::Focus { focused } => {
                self.focused = focused;
            }
            InputEvent::MouseDown { x, y } => {
                // In quickview: clicking a filmstrip chip selects it;
                // anywhere else closes. In the grid: click selects, click
                // on the selection opens quickview.
                if self.quickview {
                    if let Some(i) = self.strip_chip_at(x, y) {
                        if i != self.selected {
                            self.selected = i;
                            self.sel_changed_at = Instant::now();
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
                    } else {
                        self.selected = i;
                        self.sel_changed_at = Instant::now();
                    }
                }
            }
        }
    }

    fn frame(&mut self, dt: f32, viewport: Viewport) -> Frame {
        self.viewport = viewport;
        if let Some(cfg) = self.tuning_file.poll() {
            self.tuning = cfg.tuning;
            self.keymap = cfg.keymap;
        }
        self.drain_ingest();
        self.step(dt);
        let lay = self.layout();
        self.request_visible_thumbs(&lay);
        let mut uploads = self.drain_media(&lay);
        self.update_live(&lay, &mut uploads);
        self.update_title();
        let mut frame = self.build_frame();
        frame.uploads = uploads;
        frame.hires_upload = self.hires_frame.take();
        // Idle throttling: with nothing in motion — springs settled, no
        // sheets cycling (e.g. animation toggled off), no live video, no
        // background jobs, stdin closed — the loop drops to a slow tick.
        frame.animating = self.motion
            || self.anim_rendered
            || self.transition.is_some()
            || self.live_sel.is_some()
            || self.live_hover.is_some()
            || self.jobs_total > 0
            || self.rx.is_some()
            || Instant::now() < self.wake_until;
        frame
    }

    fn commands(&mut self) -> Vec<WindowCommand> {
        std::mem::take(&mut self.cmds)
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
