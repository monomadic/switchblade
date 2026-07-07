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

use sb_media::{MediaService, ThumbResult, ANIM_COLS, ANIM_ROWS};
use sb_window::{
    App, Frame, InputEvent, Key, ThumbUpload, Tile, Viewport, WindowCommand, ATLAS_SLOTS,
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
    Ready { slot: usize, at: Instant, tw: u32, th: u32 },
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

/// The selected clip's in-tile video playback (PLAN.md §6 level 3).
struct LiveState {
    clip: usize,
    player: sb_media::LivePlayer,
    slot: usize,
    /// Set when the first frame arrives; the tile switches to video then.
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
    thumb: Thumb,
    /// Sprite-sheet animation (M6): frames cycle in the grid; the static
    /// thumb stays authoritative for the emphasized tile.
    anim: Thumb,
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
    live: Option<LiveState>,
    /// Last time the selection changed; live playback starts once it
    /// settles for live_delay_ms.
    sel_changed_at: Instant,
    demo: bool,
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
        let rx = ingest::spawn_stdin_reader();
        let demo = rx.is_none();
        let mut app = Self {
            clips: Vec::new(),
            index: HashMap::new(),
            rx,
            media: MediaService::new(),
            slots: vec![None; ATLAS_SLOTS],
            live: None,
            sel_changed_at: Instant::now(),
            demo,
            tuning: Tuning::default(),
            keymap: KeyMap::default(),
            tuning_file: TuningFile::new(PathBuf::from("switchblade.toml")),
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
            last_scroll_event: Instant::now(),
            viewport: Viewport { width: 1280.0, height: 800.0 },
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
        let cols =
            (((self.viewport.width - t.gap) / (ideal + t.gap)).floor() as usize).max(1);
        let tile_w =
            ((self.viewport.width - t.gap * (cols as f32 + 1.0)) / cols as f32).max(1.0);
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
        let cols = lay.cols as i32;
        let rows = self.rows(&lay) as i32;
        let sel = self.selected as i32;
        let col = (sel % cols + dx).clamp(0, cols - 1);
        let row = (sel / cols + dy).clamp(0, rows - 1);
        let idx = (row * cols + col).min(self.clips.len() as i32 - 1) as usize;
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
                    self.index.insert(item.path.clone(), self.clips.len());
                    // Cloud placeholders never request a thumbnail: reading
                    // the file would force iCloud to download it.
                    let thumb = if item.cloud { Thumb::Failed } else { Thumb::None };
                    self.clips.push(Clip {
                        path: item.path,
                        readable: item.readable,
                        cloud: item.cloud,
                        spawned: Instant::now(),
                        scale: 1.0,
                        thumb,
                        anim: if item.cloud { Thumb::Failed } else { Thumb::None },
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
            self.scroll_target = max + (self.scroll_target - max) * (1.0 - alpha(t.rubber_band, dt));
            if self.scroll_target - max < 0.5 {
                self.scroll_target = max;
            }
            self.scroll_vel = 0.0;
        }

        // Camera chases its target (key moves use a gentler chase).
        self.scroll += (self.scroll_target - self.scroll) * alpha(self.chase, dt);

        self.hovered = self.tile_at(&lay, self.cursor.0, self.cursor.1);

        // Selection stands out more the further you zoom out.
        let boost = (1.0 / self.zoom.max(0.05))
            .powf(t.selection_zoom_boost)
            .clamp(1.0, 2.2);
        let sel_scale = t.selection_scale * boost;

        // Tile scale springs (selected > hover > rest).
        let a = alpha(t.scale_smoothing, dt);
        for (i, clip) in self.clips.iter_mut().enumerate() {
            let target = if i == self.selected {
                sel_scale
            } else if Some(i) == self.hovered {
                t.hover_scale
            } else {
                1.0
            };
            clip.scale += (target - clip.scale) * a;
        }
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
        let mut budget = ATLAS_SLOTS as i64 - 8; // headroom incl. live slot

        'statics: for &row in &rows {
            for col in 0..lay.cols {
                let i = row * lay.cols + col;
                let Some(clip) = self.clips.get_mut(i) else { break };
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
                }
            }
        }

        if !self.tuning.anim || lay.tile_w < self.tuning.anim_min_tile_w {
            return;
        }
        'anims: for &row in &rows {
            for col in 0..lay.cols {
                let i = row * lay.cols + col;
                let Some(clip) = self.clips.get_mut(i) else { break };
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
            match result {
                ThumbResult::Ready { path, w, h, rgba } => {
                    let Some(&i) = self.index.get(&path) else { continue };
                    let Some(slot) = self.alloc_slot(lay, SlotKind::Static) else {
                        log::debug!("static dropped, atlas full: {}", path.display());
                        self.clips[i].thumb = Thumb::Failed;
                        continue;
                    };
                    log::debug!("static ready: clip {i} -> slot {slot} ({w}x{h})");
                    self.slots[slot] = Some((i, SlotKind::Static));
                    self.clips[i].thumb =
                        Thumb::Ready { slot, at: Instant::now(), tw: w, th: h };
                    uploads.push(ThumbUpload { slot, w, h, rgba });
                }
                ThumbResult::Failed { path } => {
                    log::debug!("thumbnail failed: {}", path.display());
                    if let Some(&i) = self.index.get(&path) {
                        self.clips[i].thumb = Thumb::Failed;
                    }
                }
                ThumbResult::AnimReady { path, w, h, rgba } => {
                    let Some(&i) = self.index.get(&path) else { continue };
                    let Some(slot) = self.alloc_slot(lay, SlotKind::Anim) else {
                        log::debug!("anim dropped, atlas full: {}", path.display());
                        self.clips[i].anim = Thumb::Failed;
                        continue;
                    };
                    log::debug!("anim ready: clip {i} -> slot {slot} ({w}x{h})");
                    self.slots[slot] = Some((i, SlotKind::Anim));
                    self.clips[i].anim =
                        Thumb::Ready { slot, at: Instant::now(), tw: w, th: h };
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

    /// Live in-tile playback of the selected clip: starts once the
    /// selection settles, stops the moment it moves, and pumps the newest
    /// decoded frame into a dedicated (never-evicted) atlas slot.
    fn update_live(&mut self, lay: &Layout, uploads: &mut Vec<ThumbUpload>) {
        let stop = !self.tuning.live_preview
            || self.demo
            || self.live.as_ref().is_some_and(|l| l.clip != self.selected);
        if stop {
            if let Some(live) = self.live.take() {
                self.slots[live.slot] = None;
            }
            if !self.tuning.live_preview || self.demo {
                return;
            }
        }

        if self.live.is_none()
            && self.sel_changed_at.elapsed().as_millis() as f32 >= self.tuning.live_delay_ms
        {
            let i = self.selected;
            let Some(clip) = self.clips.get(i) else { return };
            // Decode at the static thumb's exact fit dimensions, so the
            // emphasized tile's aspect math applies unchanged.
            let (readable, cloud, tw, th, path) = match clip.thumb {
                Thumb::Ready { tw, th, .. } => {
                    (clip.readable, clip.cloud, tw, th, clip.path.clone())
                }
                _ => return,
            };
            if !readable || cloud {
                return;
            }
            let Some(slot) = self.alloc_slot(lay, SlotKind::Live) else { return };
            let Some(player) = sb_media::LivePlayer::spawn(&path, tw, th) else {
                log::debug!("live preview failed to start: {}", path.display());
                return;
            };
            log::debug!("live preview: {}", path.display());
            self.slots[slot] = Some((i, SlotKind::Live));
            self.live = Some(LiveState { clip: i, player, slot, first_frame: None });
        }

        if let Some(live) = &mut self.live {
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
                        (Some((slot, tw as f32, th as f32)), 1.0 - (1.0 - m) * (1.0 - m))
                    }
                    _ => (None, 0.0),
                };
                // The selected tile plays live video once frames arrive; it
                // decodes at the static thumb's dimensions, so it's a
                // drop-in slot swap.
                if selected {
                    if let Some(live) = &self.live {
                        if live.clip == i && live.first_frame.is_some() {
                            thumb = Some((live.slot, live.player.w as f32, live.player.h as f32));
                        }
                    }
                }

                let s = clip.scale * (0.92 + 0.08 * ease);
                let mut w = lay.tile_w * s;
                let mut h = lay.tile_h * s;

                // Emphasized tiles show the clip at its own aspect ratio,
                // capped at max_display_aspect with a centered pan-and-scan
                // crop, and sized to *cover* the scaled cell box so no
                // background peeks out behind portrait clips.
                let display_aspect = |tw: f32, th: f32| {
                    let m = t.max_display_aspect.max(1.0);
                    (tw / th).clamp(1.0 / m, m)
                };
                if emphasized {
                    if let Some((_, tw, th)) = thumb {
                        let a = display_aspect(tw, th);
                        if a > w / h {
                            w = h * a;
                        } else {
                            h = w / a;
                        }
                    }
                }
                let (ox, oy) = self.cell_origin(&lay, col, row);
                let cx = ox + lay.tile_w * 0.5;
                let cy = oy + lay.tile_h * 0.5;

                // Texture source: in the grid, a cycling anim-sheet frame
                // once available (M6); the static thumb otherwise, and
                // always for the emphasized tile (it's higher-res).
                let anim = if !emphasized && t.anim {
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
                let uv = if let Some((slot, at, sw, sh)) = anim {
                    let (cols, rows) = (ANIM_COLS as usize, ANIM_ROWS as usize);
                    let frames = (cols * rows) as f32;
                    let (fw, fh) = (sw / cols as f32, sh / rows as f32);
                    // Per-clip phase offset so neighbors don't tick in
                    // lockstep.
                    let phase = (i % 9) as f32 / frames;
                    let pos = (anim_t / t.anim_cycle_s.max(0.2) + phase).fract();
                    let k = ((pos * frames) as usize).min(cols * rows - 1);
                    let (fx, fy) = ((k % cols) as f32 * fw, (k / cols) as f32 * fh);

                    let target_a = w / h.max(1.0);
                    let (mut cw, mut ch) = (fw, fh);
                    if fw / fh > target_a {
                        cw = fh * target_a;
                    } else {
                        ch = fw / target_a;
                    }
                    let anim_fade = {
                        let m = (now.saturating_duration_since(at).as_secs_f32() / THUMB_FADE_S)
                            .min(1.0);
                        1.0 - (1.0 - m) * (1.0 - m)
                    };
                    // If the static thumb is already showing, swap without
                    // re-fading (no dip to background).
                    mix = if thumb.is_some() { mix.max(anim_fade) } else { anim_fade };
                    sb_window::atlas_uv(slot, fx + (fw - cw) * 0.5, fy + (fh - ch) * 0.5, cw, ch)
                } else {
                    // UV window into the static thumb: grid tiles crop-fill
                    // to the tile shape; emphasized tiles crop only what the
                    // aspect cap chops.
                    match thumb {
                        Some((slot, tw, th)) => {
                            let target_a = if emphasized {
                                display_aspect(tw, th)
                            } else {
                                w / h.max(1.0)
                            };
                            let (mut cw, mut ch) = (tw, th);
                            if tw / th > target_a {
                                cw = th * target_a;
                            } else {
                                ch = tw / target_a;
                            }
                            sb_window::atlas_uv(slot, (tw - cw) * 0.5, (th - ch) * 0.5, cw, ch)
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
                } else {
                    ([0.0, 0.0, 0.0], ease * mix)
                };

                let (sb, hb, eb) = (t.selection_border, t.hover_border, t.empty_border);
                let (border_color, border_width, radius) = if selected {
                    ([sb[0], sb[1], sb[2], ease], t.border_width, t.selection_corner_radius)
                } else if hovered {
                    ([hb[0], hb[1], hb[2], 0.35 * ease], 1.5, t.corner_radius)
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
                    tex_mix: mix,
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
                    push_loading_dots(out, &tile, ease, anim_t);
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

        Frame { clear: t.background, tiles, uploads: Vec::new() }
    }
}

impl App for Switchblade {
    fn event(&mut self, event: InputEvent) {
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
            InputEvent::MouseDown { x, y } => {
                let lay = self.layout();
                if let Some(i) = self.tile_at(&lay, x, y) {
                    if i != self.selected {
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
        tex_mix: 0.0,
    };
    out.push(part(bx + 2.0, by + 4.0, 13.0, 13.0)); // small bump
    out.push(part(bx + 9.0, by, 16.0, 16.0)); // big bump
    out.push(part(bx, by + 7.0, 28.0, 10.0)); // base bar
}

/// Three softly pulsing dots in the tile's bottom-right corner while its
/// thumbnail is being generated.
fn push_loading_dots(out: &mut Vec<Tile>, tile: &Tile, ease: f32, t: f32) {
    let d = 5.0;
    let gap = 4.0;
    let bx = tile.x + tile.w - (3.0 * d + 2.0 * gap) - 10.0;
    let by = tile.y + tile.h - d - 10.0;
    for k in 0..3 {
        let pulse = 0.5 + 0.5 * (t * 4.5 - k as f32 * 0.9).sin();
        out.push(Tile {
            x: bx + k as f32 * (d + gap),
            y: by,
            w: d,
            h: d,
            color: [0.55, 0.55, 0.62, (0.2 + 0.55 * pulse) * ease],
            border_color: [0.0; 4],
            corner_radius: d * 0.5,
            border_width: 0.0,
            uv: [0.0; 4],
            tex_mix: 0.0,
        });
    }
}
