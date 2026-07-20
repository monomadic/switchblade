use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;

use crate::commands::{CommandSpec, KeyMap};
use sb_media::CacheKey;

/// How much moves. Each level includes everything below it:
/// `none` = snap-everything, no tweens, no video;
/// `minimal` = UI tweens back on; `normal` (default) = live video for
/// quickview + selected/hovered. The old `full` level (background
/// sprite-sheet cycling in the grid) is gone — "full" stays accepted as
/// an alias of `normal` so existing configs keep working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnimLevel {
    None,
    Minimal,
    #[serde(alias = "full")]
    Normal,
}

impl AnimLevel {
    /// UI tweens: camera glides, springs, fades. Off = snap.
    pub fn ui(self) -> bool {
        self >= AnimLevel::Minimal
    }
    /// Live video: quickview stream + selected/hovered tiles.
    pub fn live(self) -> bool {
        self >= AnimLevel::Normal
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => AnimLevel::None,
            "minimal" => AnimLevel::Minimal,
            "normal" | "full" => AnimLevel::Normal,
            _ => return None,
        })
    }
}

/// How the grid lays tiles out: `flexible` (default) = justified rows —
/// every tile keeps its clip's true aspect ratio at the row's shared
/// height, and each row's height flexes (within `row_height_min/max` ×
/// the nominal tile height) so every row spans the full window width;
/// `fixed` = the classic uniform grid, every tile the tuning's
/// tile_width:tile_height shape, thumbs crop-filled into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GridStyle {
    #[serde(alias = "square")]
    Fixed,
    #[serde(alias = "variable", alias = "mosaic")]
    Flexible,
}

/// The grid's core gesture model (PLAN.md §15 spike). `classic` = the
/// shipped behavior: the selection plays hires, hover gets a tile-size
/// lane, click selects, click-on-selection quickviews. `attention` = one
/// hires lane follows attention (the hovered tile while mousing, the
/// selection while keyboard-navigating; the tile-size hover lane is
/// gone), a single click on any tile opens quickview by promoting that
/// stream, and cmd/shift-click multi-selects (border-only, never plays).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Interaction {
    Classic,
    Attention,
}

/// What a modal draws behind its video: `blur` = the frosted, dimmed
/// gallery (quickview's classic backdrop); `flat` = an opaque
/// `backdrop_color` stage that hides the grid entirely (fullview's
/// classic backdrop). Both modals accept either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackdropStyle {
    Blur,
    Flat,
}

/// Every feel-related constant lives here and hot-reloads from
/// `switchblade.toml` (PLAN.md §10). Don't hardcode feel values elsewhere.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Tuning {
    /// Animation level: "none" | "minimal" | "normal" | "full".
    /// CLI `--animation` overrides this.
    pub animation: AnimLevel,
    /// Grid layout: "flexible" (justified true-aspect rows, the default)
    /// or "fixed" (uniform tiles). "square"/"variable"/"mosaic" are
    /// accepted aliases.
    pub grid_layout: GridStyle,
    pub tile_width: f32,
    pub tile_height: f32,
    /// Flexible rows justify by scaling their height; these caps (as
    /// fractions of the nominal tile height) keep one pathological row
    /// from towering or vanishing. A row that would need to leave the
    /// band stops justifying and leaves a little slack at the end.
    pub row_height_min: f32,
    pub row_height_max: f32,
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
    /// Border widths of the selected / hovered tile (colors come from
    /// selection_border / hover_border below). `border_width` is accepted
    /// as a legacy alias for the selection one.
    #[serde(alias = "border_width")]
    pub selection_border_width: f32,
    pub hover_border_width: f32,
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
    /// Pause live playback and sheet animation while the window is
    /// unfocused (the grid stays visible, just still — big CPU saver).
    pub pause_unfocused: bool,
    /// Walk directories given as inputs (CLI args or stdin) for video
    /// files. Off: directories are skipped entirely. Startup-only.
    pub recurse: bool,
    /// How long the selection must settle before live playback starts.
    pub live_delay_ms: f32,
    /// Interaction model: "classic" (default) or "attention" — the
    /// PLAN.md §15 spike where one hires lane follows attention and a
    /// single click quickviews. Hot-reloadable, so the two models can be
    /// compared in place.
    pub interaction: Interaction,
    /// Attention mode only: how long a HOVER must settle before the
    /// attention lane spawns for it. Hover is more volatile than the
    /// selection and every settle costs a quickview-res cold spawn, so
    /// this guard defaults longer than `live_delay_ms`.
    pub attention_delay_ms: f32,
    /// Fraction of the clip's duration that `[`/`]` jump (0..1). A binding
    /// can override it per key via `amount` on an internal command.
    pub skip_fraction: f32,
    /// Where playback (and the matching static thumbnail) starts, as a
    /// fraction of the clip's duration (0..1) — 0.0 opens at the very
    /// beginning. Startup-only (part of the media recipe): changing it
    /// regenerates thumbs so the thumb and the first live frame agree,
    /// keeping the no-jolt handoff. The historical default 0.10 reuses
    /// the existing cache; any other value regenerates.
    pub thumb_seek_fraction: f32,
    /// Pointer travel (logical px) that turns a press on a tile or
    /// filmstrip chip into a drag-out: the clip's file leaves the app as
    /// a native macOS drag (drop into Finder or onto another app).
    /// Below this the press stays a click.
    pub drag_threshold: f32,
    /// Auto-skip (`toggle_auto_skip`): with the timer armed and quickview
    /// or fullview up, once the selected clip has played this many
    /// seconds the selection auto-advances to the next clip (wraps at
    /// the end of the library). `skip_timer_s` is the legacy spelling.
    #[serde(alias = "skip_timer_s")]
    pub auto_skip_s: f32,
    /// Radius (px) of the auto-skip timer — the arc ring in the video's
    /// top-right corner while the countdown runs.
    pub auto_skip_ring_radius: f32,
    /// false (default): the arc fills up toward the next clip (count-up).
    /// true: it drains like a classic countdown.
    pub auto_skip_countdown: bool,
    /// Background-jobs progress bar (bottom-right hairline while thumbs
    /// generate): drawn size in logical px and a master opacity (0..1)
    /// scaling both the faint track and the fill.
    pub jobs_bar_width: f32,
    pub jobs_bar_height: f32,
    pub jobs_bar_opacity: f32,
    /// Media quality — read once at startup (restart to apply).
    /// Thumbnails generate at exactly this size; any resolution works.
    /// The GPU atlas is carved into fixed slots of this same size, so
    /// bigger thumbs = fewer clips resident on the GPU at once:
    /// slots = floor(atlas_w/thumb_w) × floor(atlas_h/thumb_h).
    pub thumb_width: u32,
    pub thumb_height: u32,
    /// 1..10, 10 ≈ visually lossless, 1 = heavily compressed (maps to
    /// ffmpeg -q:v 12 - q, so even 1 stays presentable).
    pub thumb_quality: u8,
    /// Anim sheet grid (frames = grid², frame size = thumb/grid). 3 = more
    /// motion, 2 = crisper frames. Startup-only.
    pub anim_grid: u32,
    /// How cache entries are keyed to source files (startup-only):
    /// "size_mtime" (default) = size + mtime only, so entries survive
    /// renames/moves (rating-star renames, library reshuffles); "path" =
    /// absolute path + size + mtime, so a rename or move loses the cache.
    /// "size_mtime" adopts existing path-keyed entries in place — no
    /// library-wide regeneration.
    pub cache_key: CacheKey,
    /// Atlas texture dimensions (VRAM ≈ w×h×4 bytes). Clamped to 8192.
    pub atlas_width: u32,
    pub atlas_height: u32,
    /// Quickview decodes at up to this size (capped at the source's own
    /// resolution). Startup-only; higher = sharper modal, more decode CPU.
    pub quickview_max_width: u32,
    pub quickview_max_height: u32,
    /// Height of the quickview filmstrip chips (16:9).
    pub strip_height: f32,
    pub strip_gap: f32,
    /// 0..1, how hard the filmstrip chases the selection per 60fps frame
    /// (same curve family as key_snap_strength; 0.99 ≈ instant snap).
    pub strip_snap_strength: f32,
    /// Corner radius of filmstrip chips and border width of the selected
    /// chip (color comes from selection_border).
    pub strip_corner_radius: f32,
    pub strip_border_width: f32,
    /// Scale of the selected / hovered filmstrip chip (overlap is fine —
    /// they draw above their neighbors, like the grid).
    pub strip_selection_scale: f32,
    pub strip_hover_scale: f32,
    /// Quickview/fullview seekbar: pointer motion over the video reveals
    /// it; after this many seconds without motion it fades over
    /// seekbar_fade_ms.
    pub seekbar_hide_s: f32,
    pub seekbar_fade_ms: f32,
    /// Bar thickness at rest and when the pointer is on it (click target
    /// stays generous either way — the hit band is taller than the bar).
    pub seekbar_height: f32,
    pub seekbar_hover_height: f32,
    /// Opacity (0..1) of the seekbar's background track — the unfilled
    /// part behind the played fill. The fill stays solid white; this only
    /// tints the track. Scaled by the bar's reveal/fade alpha.
    pub seekbar_track_opacity: f32,
    /// Width of the storyboard preview shown while hovering the bar (the
    /// nearest anim-sheet frame to the hovered timestamp). 0 disables.
    pub seekbar_thumb_width: f32,
    /// Wheel/trackpad over the quickview filmstrip: pixels of scroll per
    /// chip-step are divided by this. Negative flips direction.
    pub strip_scroll_sensitivity: f32,
    /// Scrolling the filmstrip commits the selection to the nearest chip
    /// (magnetic, chip-by-chip flow — the default). false = peek mode:
    /// the strip pans freely so you can look at what's around while the
    /// selected clip keeps playing; click a chip to select it, and the
    /// strip re-centers whenever the selection actually changes.
    pub strip_scroll_selects: bool,
    /// Seconds a chapter step (`move_left`/`move_right` in fullview)
    /// reveals the chapter bar before it slides back down. Each step
    /// refreshes the window; a deliberate `g` open ignores this and stays
    /// up until dismissed. 0 disables the peek (the bar only opens via g).
    pub chapter_peek_s: f32,
    /// Quickview's backdrop: "blur" (frosted, dimmed gallery — default)
    /// or "flat" (opaque backdrop_color; the gallery is hidden).
    pub quickview_backdrop: BackdropStyle,
    /// Fullview's backdrop: "flat" (opaque backdrop_color — default) or
    /// "blur" (the same tinted frosted-gallery backdrop quickview uses).
    pub fullview_backdrop: BackdropStyle,
    /// Stage color behind flat-backdrop modals.
    pub backdrop_color: [f32; 3],
    /// Quickview backdrop: black-overlay strength (0..1) and frosted-glass
    /// blur level. The grid renders offscreen and is downsampled 2^level×
    /// before drawing back — a few tiny GPU passes, only while quickview
    /// is open. 0 = no blur, 1..4 = progressively softer.
    pub quickview_dim: f32,
    pub quickview_blur: f32,
    /// Fade-in duration when quickview opens (dim + blur + the modal).
    pub quickview_fade_ms: f32,
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
            animation: AnimLevel::Normal,
            grid_layout: GridStyle::Flexible,
            tile_width: 240.0,
            tile_height: 135.0,
            row_height_min: 0.62,
            row_height_max: 1.6,
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
            selection_border_width: 6.0,
            hover_border_width: 1.0,
            fade_in_ms: 220.0,
            pinch_sensitivity: 1.0,
            zoom_min: 0.35,
            zoom_max: 3.0,
            zoom_smoothing: 0.35,
            zoom_fade_ms: 180.0,
            pause_unfocused: true,
            recurse: true,
            live_delay_ms: 100.0,
            interaction: Interaction::Classic,
            attention_delay_ms: 250.0,
            skip_fraction: 0.10,
            thumb_seek_fraction: 0.10,
            drag_threshold: 6.0,
            auto_skip_s: 5.0,
            auto_skip_ring_radius: 16.0,
            auto_skip_countdown: false,
            jobs_bar_width: 84.0,
            jobs_bar_height: 4.0,
            jobs_bar_opacity: 0.8,
            thumb_width: 640,
            thumb_height: 360,
            thumb_quality: 7,
            anim_grid: 3,
            cache_key: CacheKey::SizeMtime,
            atlas_width: 7680,
            atlas_height: 4320,
            quickview_max_width: 1920,
            quickview_max_height: 1080,
            strip_height: 92.0,
            strip_gap: 10.0,
            strip_snap_strength: 0.12,
            strip_corner_radius: 5.0,
            strip_border_width: 4.0,
            strip_selection_scale: 1.35,
            strip_hover_scale: 1.15,
            seekbar_hide_s: 1.0,
            seekbar_fade_ms: 250.0,
            seekbar_height: 6.0,
            seekbar_hover_height: 12.0,
            seekbar_track_opacity: 0.28,
            seekbar_thumb_width: 190.0,
            strip_scroll_sensitivity: 1.0,
            strip_scroll_selects: true,
            chapter_peek_s: 2.5,
            quickview_backdrop: BackdropStyle::Blur,
            fullview_backdrop: BackdropStyle::Flat,
            backdrop_color: [0.0, 0.0, 0.0],
            quickview_dim: 0.90,
            quickview_blur: 3.0,
            quickview_fade_ms: 150.0,
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

/// Config search order: `./switchblade.toml` (dev/per-project override),
/// then `~/.config/switchblade.toml`, then
/// `~/.config/switchblade/config.toml` (XDG_CONFIG_HOME respected).
/// First existing file wins; if none exist we still watch the cwd path,
/// so creating it hot-loads. `switchblade --init` writes the user one.
pub fn config_path() -> PathBuf {
    let cwd = PathBuf::from("switchblade.toml");
    if cwd.exists() {
        return cwd;
    }
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(dir) = config_dir {
        for p in [
            dir.join("switchblade.toml"),
            dir.join("switchblade/config.toml"),
        ] {
            if p.exists() {
                return p;
            }
        }
    }
    cwd
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
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `grid_layout` parses both canonical names and the common aliases,
    /// and flexible is the default.
    #[test]
    fn grid_layout_parses_with_aliases() {
        assert_eq!(Tuning::default().grid_layout, GridStyle::Flexible);
        for (s, want) in [
            ("fixed", GridStyle::Fixed),
            ("square", GridStyle::Fixed),
            ("flexible", GridStyle::Flexible),
            ("variable", GridStyle::Flexible),
            ("mosaic", GridStyle::Flexible),
        ] {
            let t: Tuning = toml::from_str(&format!("grid_layout = \"{s}\"")).unwrap();
            assert_eq!(t.grid_layout, want, "{s}");
        }
    }
}
