//! sb-app: application state and grid logic, headless of any OS/GPU types.
//! Implements the `sb_window::App` trait (PLAN.md §12).

mod ingest;
mod tuning;

pub use tuning::Tuning;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use sb_window::{App, Frame, InputEvent, Key, Tile, Viewport, WindowCommand};

use tuning::{alpha, TuningFile};

const DEMO_TILES: usize = 480;

/// Derived per-frame grid metrics; see [`Switchblade::layout`].
struct Layout {
    cols: usize,
    tile_w: f32,
    tile_h: f32,
    cell_w: f32,
    cell_h: f32,
}

struct Clip {
    path: PathBuf,
    readable: bool,
    spawned: Instant,
    scale: f32,
}

pub struct Switchblade {
    clips: Vec<Clip>,
    rx: Option<Receiver<ingest::Ingested>>,
    tuning: Tuning,
    tuning_file: TuningFile,
    selected: usize,
    hovered: Option<usize>,
    cursor: (f32, f32),
    scroll: f32,
    scroll_target: f32,
    scroll_vel: f32,
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
            rx,
            tuning: Tuning::default(),
            tuning_file: TuningFile::new(PathBuf::from("switchblade.toml")),
            selected: 0,
            hovered: None,
            cursor: (0.0, 0.0),
            scroll: 0.0,
            scroll_target: 0.0,
            scroll_vel: 0.0,
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
                    // Staggered spawn cascades the fade-in across the field.
                    spawned: now + Duration::from_millis(i as u64 * 2),
                    scale: 1.0,
                });
            }
        }
        app
    }

    // --- layout ---

    /// Grid layout derived from tuning + viewport. `tuning.tile_width` is the
    /// *ideal* width used to choose the column count; tiles then stretch so
    /// the columns exactly fill the viewport and the background barely shows.
    fn layout(&self) -> Layout {
        let t = &self.tuning;
        let cols =
            (((self.viewport.width - t.gap) / (t.tile_width + t.gap)).floor() as usize).max(1);
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

    fn max_scroll(&self, lay: &Layout) -> f32 {
        let content = self.tuning.gap + self.rows(lay) as f32 * lay.cell_h;
        (content - self.viewport.height).max(0.0)
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
            _ => {}
        }
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
                    self.clips.push(Clip {
                        path: item.path,
                        readable: item.readable,
                        spawned: Instant::now(),
                        scale: 1.0,
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
        let first_row = (((self.scroll - t.gap) / lay.cell_h).floor().max(0.0)) as usize;
        let last_row = (((self.scroll + self.viewport.height) / lay.cell_h).ceil()) as usize;

        let now = Instant::now();
        let mut tiles = Vec::new();
        let mut selected_tile = None;

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

                let base = placeholder_color(i, clip.readable);
                let (border_color, border_width) = if selected {
                    ([t.accent[0], t.accent[1], t.accent[2], ease], t.border_width)
                } else if hovered {
                    ([t.accent[0], t.accent[1], t.accent[2], 0.35 * ease], 1.5)
                } else {
                    ([0.0; 4], 0.0)
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
                };
                if selected {
                    selected_tile = Some(tile);
                } else {
                    tiles.push(tile);
                }
            }
        }
        // Selected draws last, on top of scaled-up overlap.
        if let Some(tile) = selected_tile {
            tiles.push(tile);
        }

        Frame { clear: t.background, tiles }
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
        self.update_title();
        self.build_frame()
    }

    fn commands(&mut self) -> Vec<WindowCommand> {
        std::mem::take(&mut self.cmds)
    }
}

/// Deterministic placeholder tint per tile (M1 fake grid). Golden-ratio hue
/// stepping keeps neighbors visually distinct.
fn placeholder_color(i: usize, readable: bool) -> [f32; 3] {
    if !readable {
        return [0.16, 0.05, 0.06];
    }
    let hue = (i as f32 * 0.618_034).fract() * 360.0;
    let light = 0.14 + ((i * 7919) % 5) as f32 * 0.012;
    hsl_to_rgb(hue, 0.42, light)
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
