//! sb-app: application state and grid logic, headless of any OS/GPU types.
//! Implements the `sb_window::App` trait (PLAN.md §12).

mod ingest;
mod tuning;

pub use tuning::Tuning;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use sb_media::{MediaService, ThumbResult};
use sb_window::{
    App, Frame, InputEvent, Key, ThumbUpload, Tile, Viewport, WindowCommand, ATLAS_SLOTS,
};

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
    Ready { slot: usize, at: Instant },
    Failed,
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
}

pub struct Switchblade {
    clips: Vec<Clip>,
    /// Path → clip index, for routing async thumbnail results.
    index: HashMap<PathBuf, usize>,
    rx: Option<Receiver<ingest::Ingested>>,
    media: MediaService,
    /// Atlas slot → owning clip index. Fixed pool; distance-based eviction.
    slots: Vec<Option<usize>>,
    demo: bool,
    tuning: Tuning,
    tuning_file: TuningFile,
    selected: usize,
    hovered: Option<usize>,
    cursor: (f32, f32),
    scroll: f32,
    scroll_target: f32,
    scroll_vel: f32,
    zoom: f32,
    zoom_target: f32,
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
            demo,
            tuning: Tuning::default(),
            tuning_file: TuningFile::new(PathBuf::from("switchblade.toml")),
            selected: 0,
            hovered: None,
            cursor: (0.0, 0.0),
            scroll: 0.0,
            scroll_target: 0.0,
            scroll_vel: 0.0,
            zoom: 1.0,
            zoom_target: 1.0,
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
        self.selected = idx;
        self.scroll_to_selected();
    }

    /// Smoothly bring the selected row toward the vertical center.
    fn scroll_to_selected(&mut self) {
        let lay = self.layout();
        let row = self.selected / lay.cols;
        let row_center = self.tuning.gap + row as f32 * lay.cell_h + lay.tile_h * 0.5;
        self.scroll_target =
            (row_center - self.viewport.height * 0.5).clamp(0.0, self.max_scroll(&lay));
        self.scroll_vel = 0.0;
    }

    // --- input ---

    fn key(&mut self, key: Key) {
        match key {
            Key::Char('q') => self.cmds.push(WindowCommand::Quit),
            Key::Char('f') => self.cmds.push(WindowCommand::ToggleFullscreen),
            Key::Char('h') | Key::Left => self.move_selection(-1, 0),
            Key::Char('l') | Key::Right => self.move_selection(1, 0),
            Key::Char('k') | Key::Up => self.move_selection(0, -1),
            Key::Char('j') | Key::Down => self.move_selection(0, 1),
            Key::Enter | Key::Char('o') => self.action("open"),
            Key::Space => self.action("preview"),
            Key::Char('-') => self.set_zoom(self.zoom_target / 1.15),
            Key::Char('=') | Key::Char('+') => self.set_zoom(self.zoom_target * 1.15),
            Key::Char('0') => self.set_zoom(1.0),
            _ => {}
        }
    }

    fn set_zoom(&mut self, target: f32) {
        let t = &self.tuning;
        self.zoom_target = target.clamp(t.zoom_min, t.zoom_max);
    }

    fn action(&mut self, name: &str) {
        // M0/M1: log only. Real command dispatch lands at M4 (PLAN.md §11).
        if let Some(clip) = self.clips.get(self.selected) {
            log::info!("{name}: {}", clip.path.display());
        }
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

        // Camera chases its target.
        self.scroll += (self.scroll_target - self.scroll) * alpha(t.snap_strength, dt);

        self.hovered = self.tile_at(&lay, self.cursor.0, self.cursor.1);

        // Tile scale springs (selected > hover > rest).
        let a = alpha(t.scale_smoothing, dt);
        for (i, clip) in self.clips.iter_mut().enumerate() {
            let target = if i == self.selected {
                t.selection_scale
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

    /// Queue thumbnail generation for visible + nearby tiles (PLAN.md M3:
    /// prioritized by visibility/proximity — proximity via request order).
    fn request_visible_thumbs(&mut self, lay: &Layout) {
        if self.demo {
            return;
        }
        let (first_row, last_row) = self.visible_rows(lay, PREFETCH_ROWS);
        for row in first_row..=last_row {
            for col in 0..lay.cols {
                let i = row * lay.cols + col;
                let Some(clip) = self.clips.get_mut(i) else { break };
                if clip.readable && matches!(clip.thumb, Thumb::None) {
                    self.media.request(clip.path.clone());
                    clip.thumb = Thumb::Pending;
                }
            }
        }
    }

    /// Collect finished thumbnails into atlas uploads for this frame.
    fn drain_media(&mut self, lay: &Layout) -> Vec<ThumbUpload> {
        let mut uploads = Vec::new();
        while let Some(result) = self.media.try_recv() {
            match result {
                ThumbResult::Ready { path, rgba } => {
                    let Some(&i) = self.index.get(&path) else { continue };
                    let slot = self.alloc_slot(lay);
                    self.slots[slot] = Some(i);
                    self.clips[i].thumb = Thumb::Ready { slot, at: Instant::now() };
                    uploads.push(ThumbUpload { slot, rgba });
                }
                ThumbResult::Failed { path } => {
                    log::debug!("thumbnail failed: {}", path.display());
                    if let Some(&i) = self.index.get(&path) {
                        self.clips[i].thumb = Thumb::Failed;
                    }
                }
            }
        }
        uploads
    }

    /// First free atlas slot, or evict the resident clip farthest from the
    /// viewport center (it re-requests when visible again — the disk cache
    /// makes that cheap).
    fn alloc_slot(&mut self, lay: &Layout) -> usize {
        let center_row = ((self.scroll + self.viewport.height * 0.5) / lay.cell_h).max(0.0) as i64;
        let mut best: Option<(usize, i64)> = None;
        for (j, owner) in self.slots.iter().enumerate() {
            let Some(owner) = owner else { return j };
            let dist = ((owner / lay.cols) as i64 - center_row).abs();
            if best.is_none_or(|(_, bd)| dist > bd) {
                best = Some((j, dist));
            }
        }
        let (j, _) = best.expect("atlas has at least one slot");
        if let Some(victim) = self.slots[j] {
            self.clips[victim].thumb = Thumb::None;
        }
        j
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

    fn build_frame(&self) -> Frame {
        let t = &self.tuning;
        let lay = self.layout();
        let (first_row, last_row) = self.visible_rows(&lay, 0);

        let now = Instant::now();
        let mut tiles = Vec::new();
        // The selected tile (plus any badge) draws last, over neighbors.
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
                let s = clip.scale * (0.92 + 0.08 * ease);
                let w = lay.tile_w * s;
                let h = lay.tile_h * s;
                let (ox, oy) = self.cell_origin(&lay, col, row);
                let cx = ox + lay.tile_w * 0.5;
                let cy = oy + lay.tile_h * 0.5;

                let base = placeholder_color(i, clip.readable, clip.cloud);
                let (border_color, border_width) = if selected {
                    ([t.accent[0], t.accent[1], t.accent[2], ease], t.border_width)
                } else if hovered {
                    ([t.accent[0], t.accent[1], t.accent[2], 0.35 * ease], 1.5)
                } else {
                    ([0.0; 4], 0.0)
                };

                let (tex_slot, tex_mix) = match clip.thumb {
                    Thumb::Ready { slot, at } => {
                        let m = (now.saturating_duration_since(at).as_secs_f32() / THUMB_FADE_S)
                            .min(1.0);
                        (slot as i32, 1.0 - (1.0 - m) * (1.0 - m))
                    }
                    _ => (-1, 0.0),
                };

                let tile = Tile {
                    x: cx - w * 0.5,
                    y: cy - h * 0.5,
                    w,
                    h,
                    color: [base[0], base[1], base[2], ease],
                    border_color,
                    corner_radius: t.corner_radius * s,
                    border_width,
                    tex_slot,
                    tex_mix,
                };
                let out = if selected { &mut selected_group } else { &mut tiles };
                out.push(tile);
                if clip.cloud && w > 70.0 {
                    push_cloud_badge(out, &tile, ease);
                }
            }
        }
        tiles.extend(selected_group);

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
                    self.selected = i;
                }
            }
        }
    }

    fn frame(&mut self, dt: f32, viewport: Viewport) -> Frame {
        self.viewport = viewport;
        if let Some(new) = self.tuning_file.poll() {
            self.tuning = new;
        }
        self.drain_ingest();
        self.step(dt);
        let lay = self.layout();
        self.request_visible_thumbs(&lay);
        let uploads = self.drain_media(&lay);
        self.update_title();
        let mut frame = self.build_frame();
        frame.uploads = uploads;
        frame
    }

    fn commands(&mut self) -> Vec<WindowCommand> {
        std::mem::take(&mut self.cmds)
    }
}

/// Deterministic placeholder tint per tile (M1 fake grid). Golden-ratio hue
/// stepping keeps neighbors visually distinct. Cloud placeholders get a
/// dimmed blue; unreadable files a dimmed red.
fn placeholder_color(i: usize, readable: bool, cloud: bool) -> [f32; 3] {
    if cloud {
        return [0.05, 0.08, 0.13];
    }
    if !readable {
        return [0.16, 0.05, 0.06];
    }
    let hue = (i as f32 * 0.618_034).fract() * 360.0;
    let light = 0.14 + ((i * 7919) % 5) as f32 * 0.012;
    hsl_to_rgb(hue, 0.42, light)
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
        tex_slot: -1,
        tex_mix: 0.0,
    };
    out.push(part(bx + 2.0, by + 4.0, 13.0, 13.0)); // small bump
    out.push(part(bx + 9.0, by, 16.0, 16.0)); // big bump
    out.push(part(bx, by + 7.0, 28.0, 10.0)); // base bar
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [f32; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c * 0.5;
    [r + m, g + m, b + m]
}
