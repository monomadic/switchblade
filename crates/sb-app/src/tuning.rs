use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;

use crate::commands::{CommandSpec, KeyMap};

/// Every feel-related constant lives here and hot-reloads from
/// `switchblade.toml` (PLAN.md §10). Don't hardcode feel values elsewhere.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Tuning {
    pub tile_width: f32,
    pub tile_height: f32,
    pub gap: f32,
    /// Scroll delta multiplier. Negative flips direction.
    pub pan_sensitivity: f32,
    /// Extra inertia applied by us, 0..1 per-frame velocity retention at 60fps.
    /// Default 0: macOS trackpads already deliver momentum scroll events.
    pub pan_inertia: f32,
    /// 0..1, how hard the camera chases its target per 60fps frame (pan).
    pub snap_strength: f32,
    /// Same, but for keyboard selection moves — lower = gentler glide.
    pub key_snap_strength: f32,
    /// 0..1, how hard out-of-bounds scroll rubber-bands back per 60fps frame.
    pub rubber_band: f32,
    pub selection_scale: f32,
    /// Exponent boosting selection scale as you zoom out (0 = off), so the
    /// selected tile keeps standing out in a dense field.
    pub selection_zoom_boost: f32,
    pub hover_scale: f32,
    /// 0..1, how fast tile scale approaches its target per 60fps frame.
    pub scale_smoothing: f32,
    pub corner_radius: f32,
    /// Corner radius of the selected tile (usually larger).
    pub selection_corner_radius: f32,
    pub border_width: f32,
    /// Tile spawn fade/scale-in duration.
    pub fade_in_ms: f32,
    /// Pinch delta multiplier for zoom.
    pub pinch_sensitivity: f32,
    pub zoom_min: f32,
    pub zoom_max: f32,
    /// 0..1, how fast zoom approaches its target per 60fps frame.
    pub zoom_smoothing: f32,
    /// Crossfade duration when the column count reflows (Photos-style).
    pub zoom_fade_ms: f32,
    /// Live video playback inside the selected tile.
    pub live_preview: bool,
    /// How long the selection must settle before live playback starts.
    pub live_delay_ms: f32,
    /// Media quality — read once at startup (restart to apply). Thumb size
    /// doubles as the atlas slot size, so higher resolution = fewer slots:
    /// slots = floor(atlas_w/thumb_w) × floor(atlas_h/thumb_h).
    pub thumb_width: u32,
    pub thumb_height: u32,
    /// ffmpeg -q:v for thumbs/sheets: 2 ≈ visually lossless, 31 = worst.
    pub thumb_quality: u8,
    /// Anim sheet grid (frames = grid², frame size = thumb/grid). 3 = more
    /// motion, 2 = crisper frames. Startup-only.
    pub anim_grid: u32,
    /// Atlas texture dimensions (VRAM ≈ w×h×4 bytes). Clamped to 8192.
    pub atlas_width: u32,
    pub atlas_height: u32,
    /// Animated thumbnails in the grid (M6 sprite sheets).
    pub anim: bool,
    /// Seconds for one full pass through an anim sheet's frames.
    pub anim_cycle_s: f32,
    /// Portion (0..1) of each frame interval spent crossfading into the
    /// next frame; 0 = hard cuts.
    pub anim_crossfade: f32,
    /// Don't animate (or generate sheets) below this tile width — motion
    /// is invisible on tiny tiles and slots are better spent on statics.
    pub anim_min_tile_w: f32,
    /// Longest-to-shortest side cap for the selected/hovered tile's shape;
    /// clips beyond it get a centered pan-and-scan crop. 1.78 ≈ 16:9.
    pub max_display_aspect: f32,
    pub selection_border: [f32; 3],
    pub hover_border: [f32; 3],
    /// Thin outline for tiles that have no thumbnail yet.
    pub empty_border: [f32; 3],
    pub background: [f32; 3],
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            tile_width: 240.0,
            tile_height: 135.0,
            gap: 2.0,
            pan_sensitivity: 1.0,
            pan_inertia: 0.0,
            snap_strength: 0.22,
            key_snap_strength: 0.12,
            rubber_band: 0.25,
            selection_scale: 1.15,
            selection_zoom_boost: 0.35,
            hover_scale: 1.06,
            scale_smoothing: 0.35,
            corner_radius: 5.0,
            selection_corner_radius: 10.0,
            border_width: 6.0,
            fade_in_ms: 220.0,
            pinch_sensitivity: 1.0,
            zoom_min: 0.35,
            zoom_max: 3.0,
            zoom_smoothing: 0.35,
            zoom_fade_ms: 180.0,
            live_preview: true,
            live_delay_ms: 100.0,
            thumb_width: 640,
            thumb_height: 360,
            thumb_quality: 5,
            anim_grid: 3,
            atlas_width: 7680,
            atlas_height: 4320,
            anim: true,
            anim_cycle_s: 2.8,
            anim_crossfade: 0.35,
            anim_min_tile_w: 140.0,
            max_display_aspect: 1.5,
            selection_border: [0.0, 0.0, 0.0],
            hover_border: [1.0, 1.0, 1.0],
            empty_border: [0.10, 0.10, 0.13],
            background: [0.004, 0.004, 0.006],
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    tuning: Tuning,
    #[serde(default)]
    keys: HashMap<String, String>,
    #[serde(default)]
    commands: HashMap<String, CommandSpec>,
}

/// Everything the config file provides, hot-reloadable as one unit.
pub struct Config {
    pub tuning: Tuning,
    pub keymap: KeyMap,
}

/// Watches the tuning file by polling its mtime (at most every 250ms).
pub struct TuningFile {
    path: PathBuf,
    last_check: Instant,
    last_mtime: Option<SystemTime>,
}

impl TuningFile {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            // Backdate so the first poll fires immediately.
            last_check: Instant::now() - Duration::from_secs(1),
            last_mtime: None,
        }
    }

    /// Returns Some(config) when the file (re)loaded successfully.
    pub fn poll(&mut self) -> Option<Config> {
        if self.last_check.elapsed() < Duration::from_millis(250) {
            return None;
        }
        self.last_check = Instant::now();
        let mtime = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if mtime == self.last_mtime {
            return None;
        }
        self.last_mtime = mtime;
        let text = std::fs::read_to_string(&self.path).ok()?;
        match toml::from_str::<ConfigFile>(&text) {
            Ok(cfg) => {
                log::info!("config loaded from {}", self.path.display());
                Some(Config {
                    tuning: cfg.tuning,
                    keymap: KeyMap::merged(cfg.keys, cfg.commands),
                })
            }
            Err(e) => {
                log::warn!("config parse error in {}: {e}", self.path.display());
                None
            }
        }
    }
}

/// Frame-rate independent lerp factor: `k` is the fraction covered per frame
/// at 60fps; the result is the equivalent fraction for an arbitrary `dt`.
pub fn alpha(k: f32, dt: f32) -> f32 {
    1.0 - (1.0 - k.clamp(0.0, 0.999)).powf(dt * 60.0)
}
