use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;

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
    /// Strength of the rim light inside the selection border (0 = off).
    pub selection_shine: f32,
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
            hover_scale: 1.03,
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
            selection_shine: 0.5,
            selection_border: [0.0, 0.0, 0.0],
            hover_border: [1.0, 1.0, 1.0],
            empty_border: [0.16, 0.16, 0.19],
            background: [0.004, 0.004, 0.006],
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    tuning: Tuning,
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

    /// Returns Some(tuning) when the file (re)loaded successfully.
    pub fn poll(&mut self) -> Option<Tuning> {
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
                log::info!("tuning loaded from {}", self.path.display());
                Some(cfg.tuning)
            }
            Err(e) => {
                log::warn!("tuning parse error in {}: {e}", self.path.display());
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
