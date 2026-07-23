//! sb-app: application state and grid logic, headless of any OS/GPU types.
//! Implements the `sb_window::App` trait (DESIGN.md §12).

pub mod bench;
mod commands;
mod ingest;
mod tuning;

pub use tuning::{AnimLevel, BackdropStyle, GridStyle, Interaction, SortMode, Tuning, config_path};

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
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
/// Tighter still while a pinch is in flight. A gesture holds the render
/// loop at full rate for its whole duration (it IS the animation), so the
/// full budget becomes ~3800 thumb uploads a second — gigabytes of RGBA
/// staged while the user is dragging the grid around, which ground the
/// machine to a halt whenever a gen sweep was delivering. A small library
/// (nothing to deliver) never showed it. The backlog is a channel; it
/// drains the moment the gesture settles, and the sweep is background
/// work by definition — the gesture in the user's fingers is not.
const MEDIA_UPLOAD_BUDGET_GESTURE: usize = 4;
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
/// Auto-skip stall guard: when a live stream is expected but hasn't
/// delivered a first frame (cold spawn, failed decode, cloud placeholder),
/// the timer still advances after `auto_skip_s` plus this grace — a
/// slideshow must never freeze on one bad clip.
const AUTO_SKIP_SPAWN_GRACE_S: f32 = 3.0;
/// Inset of the auto-skip timer from the video's (or window's) top-right
/// corner; its radius is the `auto_skip_ring_radius` tuning.
const AUTO_SKIP_RING_MARGIN: f32 = 18.0;
/// Chapter stepping (`move_left` in fullview): more than this far into a
/// chapter, "back" restarts the chapter (DVD-player feel); within it,
/// "back" steps to the previous chapter.
const CHAPTER_RESTART_S: f64 = 2.0;
/// How far the soft dark scrim extends past the edge of the shape it
/// backs (the auto-skip ring, the seekbar) — keeps the white-on-white
/// case legible without reading as a drawn border.
const SCRIM_PAD: f32 = 3.0;
/// The scrim's black opacity — shared by the ring and the seekbar so the
/// two translucent backings always match.
const SCRIM_ALPHA: f32 = 0.45;
/// The seekbar's scrim runs thicker than the ring's — a chunky dark cuff
/// around the line, so the white track/fill read clearly over any video.
const SEEKBAR_SCRIM_PAD: f32 = 4.0;
/// Accumulated pinch magnitude (summed trackpad deltas) that steps a modal
/// view up or down the depth ladder.
const MODAL_PINCH_STEP: f32 = 0.18;

const DEMO_TILES: usize = 480;

/// Derived per-frame grid metrics; see [`Switchblade::layout`].
/// `cols`/`tile_*`/`cell_*` are the fixed grid's uniform cells — in
/// flexible mode they stay as the NOMINAL metrics (reflow-crossfade
/// trigger, anim-size gate, budget estimates) and `flex` carries the
/// real per-row geometry. All position/hit/row math goes through the
/// mode-agnostic helpers (`tile_rect`, `row_of`, `row_range`,
/// `row_at_y`), never through the uniform fields directly.
struct Layout {
    cols: usize,
    tile_w: f32,
    tile_h: f32,
    cell_w: f32,
    cell_h: f32,
    /// Justified rows (grid_layout = "flexible"); None = fixed grid.
    flex: Option<Rc<FlexGrid>>,
}

/// The flexible ("justified"/mosaic) grid: rows of true-aspect tiles at
/// a shared per-row height, each row's height flexed so it spans the
/// viewport width. Rebuilt only when its inputs change (viewport width,
/// zoom, clip count, an aspect landing, grid tuning) — see `flex_grid`.
struct FlexGrid {
    rows: Vec<FlexRow>,
    /// Content height including the trailing gap.
    height: f32,
}

struct FlexRow {
    /// Clips `start..end` in library order.
    start: usize,
    end: usize,
    /// Content-space top edge (scroll not applied) and row height.
    y: f32,
    h: f32,
    /// Per-tile left edge and width, indexed by `i - start`.
    x: Vec<f32>,
    w: Vec<f32>,
}

/// Per-clip zoom-reflow animation for the flexible grid (see `step_reflow`).
/// `rect` is the smoothed content-space rect currently displayed — it chases
/// the layout's `tile_rect` so a re-justify or slot shift glides instead of
/// popping. When the clip crosses a row boundary a `WrapEvent` starts: the
/// tile slides off one window edge while the same clip re-enters on the new
/// row from the opposite edge. Indexed by clip; reseeded on `grid_rev` bumps.
#[derive(Clone, Copy, Default)]
struct TileReflow {
    /// Smoothed displayed rect (x, y, w, h), content space (scroll not applied).
    rect: [f32; 4],
    /// Row the clip is settling into.
    row: u32,
    /// False until seeded from a layout — fresh/reset/off-screen clips snap.
    init: bool,
    /// Active row crossing, if any.
    wrap: Option<WrapEvent>,
}

#[derive(Clone, Copy)]
struct WrapEvent {
    /// The rect the clip is leaving (content space) — the exit copy's anchor.
    from: [f32; 4],
    /// Milliseconds elapsed, accumulated from `dt` (not wall-clock) so the
    /// slide stays frame-rate independent. Reaches `zoom_wrap_ms` to finish.
    elapsed: f32,
    /// True = crossing to a later row (zoom-in): exit right, enter from left.
    /// False = earlier row (zoom-out): exit left, enter from right.
    forward: bool,
}

/// A live pinch in the flexible grid (`zoom_ribbon`). The library is one
/// long strip of true-aspect chips wrapped at the window width, gripped at
/// a point ON the chip under the fingers: zoom scales the strip about that
/// grip, so the gripped chip stays put, neighbours expand outward, chips at
/// a row edge clip against the window, and a chip pushed past one edge is
/// simultaneously entering the adjacent row — wrapping is continuous, with
/// no discrete pop. Rows walk outward from the grip row and each is laid as
/// ONE joined strip at its own height, so a chip visiting a row is a full
/// member of it (same height, gap-joined) and can never overlap its
/// rowmates or leave a hole. Released, it settles onto the nearest SAFE
/// layout (`ribbon_quantize`) rather than being slung back to a justified
/// packing — see `RibbonPack`.
struct RibbonGrip {
    /// The gripped clip and where on it the fingers landed (0..1 of its rect).
    clip: usize,
    fx: f32,
    fy: f32,
    /// The grip point in viewport coords — the chip is pinned here.
    cx: f32,
    cy: f32,
    /// Zoom when the grip was taken; drives both the scale and the blend.
    z0: f32,
    /// Row the grip started in, and the resting row heights at that moment:
    /// rows blend from these toward the ribbon's uniform height, so a
    /// gesture that has barely moved leaves the grid exactly as it was.
    grip_row: usize,
    rest_h: Vec<f32>,
    /// Last pinch event — the gesture ends `zoom_ribbon_release_ms` after it
    /// (trackpad pinches arrive as a stream of deltas with no end event).
    last: Instant,
}

/// One chip copy inside a ribbon row. A chip straddling a row boundary
/// appears in both rows (two copies, complementary clipping).
#[derive(Clone, Copy)]
struct RibbonItem {
    clip: usize,
    x: f32,
    w: f32,
}

/// A ribbon row: viewport-space top edge, shared height, and its members
/// laid left to right exactly one gap apart.
#[derive(Clone)]
struct RibbonRow {
    y: f32,
    h: f32,
    items: Vec<RibbonItem>,
}

/// The resting packing a ribbon gesture settled into: row START indices
/// only. Geometry is rebuilt from them (`build_ribbon_flex`) so a thumb or
/// aspect arrival re-justifies in place instead of drifting from what the
/// user settled on; if the membership stops fitting, the pack is dropped
/// and the plain justified grid takes over. Encoding membership as starts
/// also makes rows a partition by construction — no clip can end up in two
/// rows at rest.
#[derive(Clone)]
struct RibbonPack {
    starts: Vec<usize>,
    /// The viewport width and zoom it was packed for; either changing
    /// invalidates it (a resize or keyboard zoom returns to justified).
    vw: f32,
    zoom: f32,
}

/// The release morph: every row glides from where the gesture left it to
/// its quantized resting slot. Both ends are gap-joined strips and the
/// interpolation is linear, so every intermediate frame is a gap-joined
/// strip too — joins, uniform row heights and row spacing hold for free
/// instead of being animated toward.
struct RibbonSettle {
    from: Vec<RibbonRow>,
    to: Vec<RibbonRow>,
    /// 0..1 progress.
    t: f32,
    /// Installed when the morph lands, together with `scroll`.
    pack: RibbonPack,
    scroll: f32,
}

/// Cache key for the flexible layout — every input that shapes it.
/// f32 inputs are compared by bit pattern.
#[derive(Clone, Copy, PartialEq)]
struct FlexKey {
    vw: u32,
    zoom: u32,
    len: usize,
    /// Bumped on aspect arrivals, shuffles and library swaps.
    rev: u64,
    tile_w: u32,
    tile_h: u32,
    gap: u32,
    rmin: u32,
    rmax: u32,
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
    /// Set when the bar was revealed as a transient peek (a fullview
    /// chapter step), cleared for a deliberate `g` open. Once this
    /// instant passes the bar auto-closes; each chapter step refreshes
    /// it. A peeked bar also doesn't intercept Esc — it's ephemeral.
    peek_until: Option<Instant>,
    /// The chapter the user last navigated TO (step / chip click) — the
    /// intent that pins the highlight and the next step's base. Keyframe
    /// seeks land on the nearest keyframe *before* the chapter start, so
    /// the decoder position sits in the previous chapter's tail; deriving
    /// the playing chapter purely from position would jank the highlight
    /// back to the old chip and make forward stepping stuck. `nav` holds
    /// the intended chapter until playback carries past it (or a move
    /// lands elsewhere) — see `resolve_chapter`.
    nav: Option<usize>,
}

/// The hovered tile's video playback: tile-sized, into an atlas slot.
/// Rides `SeekablePlayer` like every live lane since the libav port (it
/// never seeks, but the in-process spawn reaches first frame ~2× sooner
/// than the old CLI pipe — hover-play is all about that latency).
struct LiveState {
    clip: usize,
    player: sb_media::SeekablePlayer,
    slot: usize,
    /// Set when the first frame arrives; the tile switches to video then.
    first_frame: Option<Instant>,
    /// Lane-incarnation id for benchmark events (phase-0-contracts §0.1).
    generation: u64,
    /// Frames served so far — drives the SB_HANDOFF_DUMP field
    /// instrumentation (first few frames only) at zero steady-state cost.
    served: u32,
}

/// The selected clip's live stream: decoded once at quickview resolution
/// into the mipmapped hires texture. The tile shows it downscaled and the
/// quickview modal shows it big — one decoder, one timeline, no handoffs.
/// Rides the resident in-process decoder (DESIGN.md §15 "Low-latency seek"),
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
    /// Lane-incarnation id for benchmark events (phase-0-contracts §0.1).
    generation: u64,
    /// Frames served so far — drives the SB_HANDOFF_DUMP field
    /// instrumentation (first few frames only) at zero steady-state cost.
    served: u32,
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
    /// Displayed aspect (w/h), learned when the static thumb first
    /// delivers (fit-scaled, rotation applied) and kept for the session —
    /// atlas eviction must not reflow the flexible grid or reshape strip
    /// chips, so this outlives `Thumb::Ready`. None until known (16:9
    /// assumed).
    aspect: Option<f32>,
    /// Creation time from the ingest thread's stat (birthtime, mtime
    /// fallback) — the sorted-ingest merge key. None for demo tiles and
    /// cloud placeholders.
    created: Option<std::time::SystemTime>,
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
    /// Inject a fully-resolved `Tuning` (the bench runner's scenario
    /// overrides). When set, no config file is loaded and nothing
    /// hot-reloads — the injected values are exactly what runs.
    pub tuning: Option<Tuning>,
    /// `--no-storyboards`: never generate the on-demand storyboard sheet
    /// (the g² anim atlas the seekbar skimming + chapter chips sample).
    /// For A/B testing the feature and for machines where the nine niced
    /// ffmpeg extracts per opened clip aren't worth it.
    pub no_storyboards: bool,
    /// `--sort <none|newest|oldest>`: overrides the config's `sort` —
    /// the gatekeeper's at-open ordering (see `SortMode`).
    pub sort: Option<SortMode>,
}

pub struct Switchblade {
    clips: Vec<Clip>,
    /// Path → clip index, for routing async thumbnail results.
    index: HashMap<PathBuf, usize>,
    rx: Option<Receiver<ingest::Ingested>>,
    media: MediaService,
    /// Benchmark instrumentation (benchmarks/design/phase-0-contracts.md).
    /// Always present so counters tally in every run; event recording is
    /// armed only under the bench runner. Cloned into each live decoder
    /// lane so its media thread can emit identity-tagged events.
    probe: Arc<sb_media::Probe>,
    /// Monotonic lane-incarnation id, minted per decoder spawn. Stamped on
    /// every event so a late frame from an obsolete lane is never credited
    /// to the current action (phase-0-contracts §0.1).
    next_lane_gen: u64,
    /// Atlas slot → owner. Fixed pool shared by static thumbs, anim sheets
    /// and the live frame; class+distance-based eviction (see alloc_slot).
    slots: Vec<Option<(usize, SlotKind)>>,
    /// Live playback: the selected clip's hires stream + the hovered
    /// tile's atlas-slot lane, each started once its target settles.
    live_sel: Option<SelLive>,
    live_hover: Option<LiveState>,
    /// Continuity handoff (clip index, content position): where the hover
    /// preview is/was playing, refreshed while the lane lives and kept
    /// after it dies for as long as the selection stays on that clip. A
    /// selected-lane open for the clip continues from here instead of
    /// restarting at the thumb anchor — clicking a tile you watched
    /// preview for 6s used to jump playback 6s backward (the warm stream
    /// spawned at the anchor and sat parked; measured in
    /// benchmarks/scenarios/hover_then_select_handoff.toml).
    hover_resume: Option<(usize, f64)>,
    /// Pre-warmed decoders for the filmstrip neighbors (quickview only),
    /// spawned ahead of need so h/l shows video the same tick. An
    /// unwatched player's bounded frame queue fills and stalls its
    /// decoder after a few frames, so warmth is all but free.
    warm: Vec<SelLive>,
    /// The newest hires frame this tick, routed to Frame.hires_upload.
    hires_frame: Option<HiresFrame>,
    /// The app's handle on the last presented hires buffer (P1.5): once
    /// the renderer drops its `Frame` (Arc count back to 1), the ~33MB
    /// buffer goes to the selected player's recycle pool instead of the
    /// allocator — steady playback then reuses the same few buffers.
    hires_reclaim: Option<Arc<Vec<u8>>>,
    /// Which clip's pixels the hires texture currently holds. Lets a
    /// mid-seek stream keep showing its last frame (the texture still
    /// has it) instead of flashing back to the thumbnail while the new
    /// position decodes in.
    hires_shown: Option<PathBuf>,
    /// The D (siblings) swap in flight: the selected clip's path, kept
    /// playing while the parent-dir listing streams in; when it arrives
    /// it becomes the selection again.
    pending_reselect: Option<PathBuf>,
    /// The gatekeeper's at-open ordering (`--sort` / config `sort`,
    /// resolved at startup): arrivals merge into a creation-date-sorted
    /// grid instead of appending.
    sort: SortMode,
    /// Sorted merging stays on only while the library's order IS the
    /// sorted order: a shuffle takes ownership of the arrangement and
    /// disarms it (arrivals append after the shuffled block, as ever); a
    /// D-swap rebuild starts a fresh library and re-arms it.
    sort_armed: bool,
    /// Paths whose live decoder failed, and when — spawn attempts wait
    /// out `LIVE_RETRY_COOLDOWN_S` (P0.4).
    live_retry: HashMap<PathBuf, Instant>,
    /// The selected stream is parked: its tile is offscreen and no modal
    /// shows it, so frames aren't drained (bounded backpressure stalls
    /// the decoder) and `animating` ignores the lane (P0.4).
    sel_parked: bool,
    /// Set on `[`/`]` — the skip flash bar shows for a moment after.
    skip_flash_at: Option<Instant>,
    /// Auto-skip (`toggle_auto_skip`): when armed, holds the countdown's
    /// re-anchor instant — refreshed while the countdown is suspended
    /// (grid view, pauses, scrubs), so a clip only ever counts down from
    /// the moment it's actually being watched. None = off.
    auto_skip_since: Option<Instant>,
    /// Clips already queued for meta.json healing this session (old
    /// cache entries lack pix_fmt, so live spawns fall back to the
    /// software chain until a background reprobe rewrites them — see
    /// `MediaService::request_reprobe`). Keeps repeat visits from
    /// re-queueing the same clip.
    reprobed: std::collections::HashSet<PathBuf>,
    /// Session memo of complete cached_meta results by path (P0.7): live
    /// spawns run on the render thread, so the disk read behind them
    /// (source stat + meta.json) is paid at most once per clip — every
    /// warm/re-spawn afterwards is a pure memory hit. Incomplete metas
    /// (no pix_fmt: pre-heal cache entries) are deliberately NOT
    /// memoized — they re-read each spawn, which is how the background
    /// reprobe's heal gets picked up without a cross-thread signal.
    meta_cache: HashMap<PathBuf, sb_media::Meta>,
    sel_changed_at: Instant,
    hover_changed_at: Instant,
    demo: bool,
    /// `--no-storyboards`: gate every storyboard-sheet request off, so no
    /// anim atlas is ever generated (seekbar skimming + chapter chips fall
    /// back to no preview). Startup-only.
    no_storyboards: bool,
    /// CLI `--animation` override; beats the config's level when set.
    cli_animation: Option<AnimLevel>,
    /// Window focus state + the runtime toggle for pause-when-unfocused.
    focused: bool,
    focus_pause_on: bool,
    anim_grid: u32,
    /// Where live playback seeks to on spawn (fraction of duration),
    /// captured from the media recipe at startup so it stays locked to the
    /// fraction the cached thumbnail was extracted at — the no-jolt handoff.
    seek_fraction: f64,
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
    /// Multi-selected clips (attention mode's cmd/shift-click): border-only
    /// state — marked clips never play. Index-keyed like the selection, so
    /// shuffle remaps it and the D swap clears it.
    marked: std::collections::HashSet<usize>,
    /// Attention mode: which input owns the attention lane. True after
    /// pointer movement (attention = the hovered tile), false after a
    /// keyboard selection move (attention = the selection).
    mouse_attention: bool,
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
    /// Memoized flexible-grid geometry: rebuilding is O(library), so it
    /// only happens when an input in `FlexKey` actually changed.
    /// RefCell because `layout()` is called from `&self` contexts.
    flex_cache: RefCell<Option<(FlexKey, Rc<FlexGrid>)>>,
    /// Bumped whenever flexible-layout inputs outside `FlexKey`'s plain
    /// fields change: an aspect arrival, a shuffle, a library swap.
    grid_rev: u64,
    /// Previous frame's tiles + column count, for the reflow crossfade.
    last_cols: usize,
    last_tiles: Vec<Tile>,
    transition: Option<(Vec<Tile>, Instant)>,
    /// Per-clip zoom-reflow state (flexible grid + `zoom_wrap`): smoothed
    /// displayed rects + active row-wrap events. Empty when the wrap path is
    /// inactive (fixed grid, modal, `zoom_wrap` off). Deliberately NOT reset
    /// on `grid_rev` (thumb arrivals bump it constantly); the index-remap
    /// sites (shuffle, D swap) clear it explicitly instead.
    reflow: Vec<TileReflow>,
    /// Ribbon pinch state (flexible grid + `zoom_ribbon`): the live grip,
    /// the release morph, and the resting packing a settled gesture left
    /// behind (which overrides the justified layout until a keyboard zoom,
    /// resize or index remap drops it).
    ribbon: Option<RibbonGrip>,
    ribbon_settle: Option<RibbonSettle>,
    ribbon_pack: Option<RibbonPack>,
    /// Scratch for the ribbon draw table (clip → its rect, plus a second
    /// copy while it straddles a row boundary), REUSED across frames and
    /// cleared only where it was written. Allocating these per frame cost
    /// two library-sized vectors every frame of a gesture — on a large
    /// library that is hundreds of MB/s of churn, which is what the app
    /// eventually choked on.
    ribbon_main: Vec<Option<[f32; 4]>>,
    ribbon_second: Vec<Option<[f32; 4]>>,
    /// App start, the time base for looping micro-animations (loading dots).
    t0: Instant,
    /// Springs still in flight this frame (drives idle throttling).
    motion: bool,
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
    /// Accumulated pinch delta while a modal is up — a pinch there steps
    /// the view-depth ladder (out = shallower, in = deeper) once the
    /// magnitude crosses `MODAL_PINCH_STEP`, rather than zooming the grid.
    modal_pinch_accum: f32,
    viewport: Viewport,
    cmds: Vec<WindowCommand>,
    title: String,
    /// A mouse press on a grid tile or filmstrip chip that may still
    /// become a drag-out: pointer travel past `drag_threshold` matures
    /// it into a `WindowCommand::BeginDrag` (and the press stops being a
    /// click); otherwise MouseUp resolves it — including the quickview
    /// open for a press on the selection, which waits for release
    /// exactly so a drag never opens the modal.
    press: Option<Press>,
}

/// See [`Switchblade::press`].
struct Press {
    x: f32,
    y: f32,
    clip: usize,
    /// The press was on the already-selected grid tile: releasing
    /// without dragging opens quickview.
    open_quickview: bool,
}

/// Once-a-second debug log of how many frames ran and which condition
/// kept `Frame.animating` true — the acceptance instrument for idle-
/// throttling work (docs/perf-reviews/02-efficiency-review.md P0.2): it distinguishes visual
/// animation, live playback, and explicit wake timers as redraw causes.
/// Negligible when debug logging is off (one `log_enabled!` check).
struct RedrawStats {
    at: Instant,
    frames: u32,
    idle: u32,
    motion: u32,
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

    fn record(&mut self, motion: bool, transition: bool, live: bool, timer: bool, animating: bool) {
        if !log::log_enabled!(log::Level::Debug) {
            return;
        }
        self.frames += 1;
        self.idle += u32::from(!animating);
        self.motion += u32::from(motion);
        self.transition += u32::from(transition);
        self.live += u32::from(live);
        self.timer += u32::from(timer);
        if self.at.elapsed() >= Duration::from_secs(1) {
            log::debug!(
                "redraw: {} frames/s ({} idle; causes: motion {} transition {} live {} timer {}); atlas slots used {} / zone demand {}",
                self.frames,
                self.idle,
                self.motion,
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
        seek_fraction: tuning.thumb_seek_fraction.clamp(0.0, 0.99),
    }
}

impl Switchblade {
    pub fn new() -> Self {
        Self::with_options(Options::default())
    }

    /// The benchmark instrumentation handle (benchmarks/design/phase-0-contracts.md).
    /// The bench runner arms event recording on it, samples counters each
    /// tick, and drains events at the end; normal runs never touch it.
    pub fn probe(&self) -> Arc<sb_media::Probe> {
        self.probe.clone()
    }

    pub fn with_options(opts: Options) -> Self {
        // Load config up front: atlas geometry, the media recipe, and the
        // ingest recurse flag are startup-only (the rest keeps
        // hot-reloading per frame). `--no-config` skips the file entirely
        // — internal defaults, nothing watched.
        // An injected `Tuning` (the bench runner's scenario overrides) wins
        // outright: no config file is loaded and nothing hot-reloads, so a
        // scenario's knobs are exactly what runs. Otherwise the usual file
        // load (unless `--no-config`).
        let mut tuning_file = (opts.tuning.is_none() && !opts.no_config)
            .then(|| TuningFile::new(tuning::config_path()));
        let (tuning, keymap) = match opts.tuning.clone() {
            Some(t) => (t, KeyMap::default()),
            None => match tuning_file.as_mut().and_then(|f| f.poll()) {
                Some(cfg) => (cfg.tuning, cfg.keymap),
                None => (Tuning::default(), KeyMap::default()),
            },
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
                tuning.gatekeeper,
                notify.clone(),
            ))
        } else if opts.demo {
            None
        } else {
            ingest::spawn_stdin_reader(tuning.recurse, tuning.gatekeeper, notify.clone())
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
            media: MediaService::new(recipe, notify.clone(), tuning.gen_live_concurrency),
            slots: vec![None; atlas_cfg.slots()],
            live_sel: None,
            live_hover: None,
            hover_resume: None,
            warm: Vec::new(),
            hires_frame: None,
            hires_shown: None,
            hires_reclaim: None,
            pending_reselect: None,
            sort: opts.sort.unwrap_or(tuning.sort),
            sort_armed: opts.sort.unwrap_or(tuning.sort) != SortMode::None,
            live_retry: HashMap::new(),
            sel_parked: false,
            skip_flash_at: None,
            auto_skip_since: None,
            reprobed: std::collections::HashSet::new(),
            meta_cache: HashMap::new(),
            sel_changed_at: Instant::now(),
            hover_changed_at: Instant::now(),
            demo,
            no_storyboards: opts.no_storyboards,
            cli_animation: opts.animation,
            focused: true,
            focus_pause_on: true,
            anim_grid: recipe.anim_grid,
            seek_fraction: recipe.seek_fraction as f64,
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
            marked: std::collections::HashSet::new(),
            mouse_attention: false,
            cursor: (0.0, 0.0),
            scroll: 0.0,
            scroll_target: 0.0,
            scroll_vel: 0.0,
            zoom: 1.0,
            zoom_target: 1.0,
            chase: 0.22,
            frozen_grid: None,
            flex_cache: RefCell::new(None),
            grid_rev: 0,
            last_cols: 0,
            last_tiles: Vec::new(),
            transition: None,
            reflow: Vec::new(),
            ribbon: None,
            ribbon_settle: None,
            ribbon_pack: None,
            ribbon_main: Vec::new(),
            ribbon_second: Vec::new(),
            t0: Instant::now(),
            motion: true,
            wake_until: Instant::now() + Duration::from_secs(1),
            waker,
            notify,
            probe: sb_media::Probe::new(),
            next_lane_gen: 0,
            redraw_stats: RedrawStats::new(),
            last_scroll_event: Instant::now(),
            modal_pinch_accum: 0.0,
            viewport: Viewport {
                width: 1280.0,
                height: 800.0,
            },
            cmds: Vec::new(),
            title: String::new(),
            press: None,
        };
        // Queued now, drained by the window layer right after the window
        // exists — so --fullscreen never flashes a windowed frame.
        if let Some(fast) = opts.fullscreen {
            app.cmds.push(WindowCommand::ToggleFullscreen { fast });
        }
        if demo {
            // `SB_DEMO_TILES` scales the fake library — the only way to
            // measure layout/gesture cost against library size headlessly
            // (sb-bench has no 10k-file corpus).
            let tiles = std::env::var("SB_DEMO_TILES")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(DEMO_TILES);
            log::info!("stdin is a tty — demo mode with {tiles} fake tiles");
            let now = Instant::now();
            for i in 0..tiles {
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
                    // A varied spread so --demo shows off the flexible
                    // layout: mostly landscape, some portrait/square.
                    aspect: Some(
                        [
                            16.0 / 9.0,
                            9.0 / 16.0,
                            16.0 / 9.0,
                            1.0,
                            4.0 / 3.0,
                            2.35,
                            16.0 / 9.0,
                        ][i % 7],
                    ),
                    created: None,
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
    /// The grid layout. While a ribbon pinch is live it is FROZEN at the
    /// zoom the grip was taken at: the gesture draws its own rows, so the
    /// layout underneath only feeds hit-tests, atlas budgets and warm
    /// neighbours — and letting those follow the mid-gesture zoom repacked
    /// the whole library every frame (an O(library) rebuild per pinch
    /// event, every one a `FlexKey` miss) and churned every decision they
    /// drive: a warm decoder dropped and respawned per frame, the hover
    /// lane likewise, atlas slots evicted and reloaded. That is what
    /// ground the machine down mid-gesture — the app-side layout cost is
    /// only the visible half. The settle runs at the final zoom, so it
    /// rebuilds once and then stays cached.
    fn layout(&self) -> Layout {
        let zoom = self.ribbon.as_ref().map_or(self.zoom, |g| g.z0);
        self.layout_with(zoom)
    }

    fn layout_with(&self, zoom: f32) -> Layout {
        let t = &self.tuning;
        let ideal = (t.tile_width * zoom).max(40.0);
        let cols = (((self.viewport.width - t.gap) / (ideal + t.gap)).floor() as usize).max(1);
        let tile_w = ((self.viewport.width - t.gap * (cols as f32 + 1.0)) / cols as f32).max(1.0);
        let tile_h = tile_w * t.tile_height / t.tile_width.max(1.0);
        let flex = (t.grid_layout == GridStyle::Flexible).then(|| self.flex_grid(zoom));
        Layout {
            cols,
            tile_w,
            tile_h,
            cell_w: tile_w + t.gap,
            cell_h: tile_h + t.gap,
            flex,
        }
    }

    /// The flexible grid for this zoom, memoized until an input changes
    /// (`FlexKey`): a zoom glide or thumb-burst rebuilds per change, but
    /// a settled grid costs one key comparison per `layout()` call.
    fn flex_grid(&self, zoom: f32) -> Rc<FlexGrid> {
        let t = &self.tuning;
        let key = FlexKey {
            vw: self.viewport.width.to_bits(),
            zoom: zoom.to_bits(),
            len: self.clips.len(),
            rev: self.grid_rev,
            tile_w: t.tile_width.to_bits(),
            tile_h: t.tile_height.to_bits(),
            gap: t.gap.to_bits(),
            rmin: t.row_height_min.to_bits(),
            rmax: t.row_height_max.to_bits(),
        };
        let mut cache = self.flex_cache.borrow_mut();
        if let Some((k, grid)) = cache.as_ref()
            && *k == key
        {
            return grid.clone();
        }
        // A settled ribbon gesture owns the resting packing: rebuild its
        // rows from their membership (so an aspect arrival re-justifies in
        // place) and only fall back to the justified packing if that
        // membership no longer fits the window.
        let grid = self
            .ribbon_pack
            .as_ref()
            .filter(|p| p.vw == self.viewport.width && p.zoom == zoom)
            .and_then(|p| self.build_ribbon_flex(&p.starts, zoom))
            .unwrap_or_else(|| self.build_flex(zoom));
        let grid = Rc::new(grid);
        *cache = Some((key, grid.clone()));
        grid
    }

    /// Tile height a row would have at this zoom before any justification —
    /// the ribbon's uniform strip height, and the flexible grid's nominal.
    fn nominal_h(&self, zoom: f32) -> f32 {
        let t = &self.tuning;
        let ideal_w = (t.tile_width * zoom).max(40.0);
        (ideal_w * t.tile_height / t.tile_width.max(1.0)).max(24.0)
    }

    /// Justified layout: rows fill greedily with true-aspect tiles at
    /// the nominal height, then each row scales so it exactly spans the
    /// viewport width — capped to `row_height_min/max` × nominal, so a
    /// capped row leaves a little slack instead of ballooning. The last
    /// (underfull) row never grows past nominal height.
    fn build_flex(&self, zoom: f32) -> FlexGrid {
        let t = &self.tuning;
        let g = t.gap;
        let vw = self.viewport.width;
        let nominal_h = self.nominal_h(zoom);
        let rmin = t.row_height_min.clamp(0.1, 1.0);
        let rmax = t.row_height_max.clamp(1.0, 4.0);
        // Width available to `cnt` tiles (side margins + inner gaps).
        let usable = |cnt: f32| (vw - g * (cnt + 1.0)).max(1.0);
        let aspect = |i: usize| self.chip_aspect(i);

        let n = self.clips.len();
        let mut rows = Vec::new();
        let mut y = g;
        let mut i = 0;
        while i < n {
            let start = i;
            let mut sum = nominal_h * aspect(i);
            i += 1;
            // Fill while the row still fits at nominal height.
            while i < n {
                let w = nominal_h * aspect(i);
                let cnt = (i - start) as f32;
                if sum + w > usable(cnt + 1.0) {
                    break;
                }
                sum += w;
                i += 1;
            }
            // The overflowing clip still joins when shrinking with it
            // lands nearer nominal than growing without it — and never
            // below the min cap (that would overflow the right edge).
            if i < n {
                let cnt = (i - start) as f32;
                let w = nominal_h * aspect(i);
                let s_with = usable(cnt + 1.0) / (sum + w);
                let s_without = usable(cnt) / sum;
                if s_with >= rmin && s_with.ln().abs() < s_without.ln().abs() {
                    sum += w;
                    i += 1;
                }
            }
            let cnt = (i - start) as f32;
            let raw = usable(cnt) / sum;
            let scale = if i == n {
                raw.clamp(rmin, 1.0)
            } else {
                raw.clamp(rmin, rmax)
            };
            let h = nominal_h * scale;
            let mut x = Vec::with_capacity(i - start);
            let mut w = Vec::with_capacity(i - start);
            let mut cx = g;
            for j in start..i {
                x.push(cx);
                let tw = nominal_h * aspect(j) * scale;
                w.push(tw);
                cx += tw + g;
            }
            rows.push(FlexRow {
                start,
                end: i,
                y,
                h,
                x,
                w,
            });
            y += h + g;
        }
        FlexGrid { rows, height: y }
    }

    fn rows(&self, lay: &Layout) -> usize {
        match &lay.flex {
            Some(f) => f.rows.len(),
            None => self.clips.len().div_ceil(lay.cols),
        }
    }

    /// Row containing clip `i` (both modes).
    fn row_of(&self, lay: &Layout, i: usize) -> usize {
        match &lay.flex {
            Some(f) => f.rows.partition_point(|r| r.start <= i).saturating_sub(1),
            None => i / lay.cols.max(1),
        }
    }

    /// Clip indices in `row` (clamped to the library; empty when out of
    /// range).
    fn row_range(&self, lay: &Layout, row: usize) -> std::ops::Range<usize> {
        match &lay.flex {
            Some(f) => f.rows.get(row).map(|r| r.start..r.end).unwrap_or(0..0),
            None => {
                let s = (row * lay.cols).min(self.clips.len());
                s..(s + lay.cols).min(self.clips.len())
            }
        }
    }

    /// Content-space rect `(x, y, w, h)` of tile `i` (scroll NOT applied).
    fn tile_rect(&self, lay: &Layout, i: usize) -> (f32, f32, f32, f32) {
        match &lay.flex {
            Some(f) => {
                let g = self.tuning.gap;
                let Some(r) = f.rows.get(self.row_of(lay, i)) else {
                    return (g, g, lay.tile_w, lay.tile_h); // empty grid
                };
                let j = i - r.start;
                if j >= r.x.len() {
                    return (g, r.y, lay.tile_w, r.h); // defensive: index past the row
                }
                (r.x[j], r.y, r.w[j], r.h)
            }
            None => {
                let g = self.tuning.gap;
                let cols = lay.cols.max(1);
                (
                    g + (i % cols) as f32 * lay.cell_w,
                    g + (i / cols) as f32 * lay.cell_h,
                    lay.tile_w,
                    lay.tile_h,
                )
            }
        }
    }

    /// Row whose band contains content-space `y` (clamped to the ends).
    fn row_at_y(&self, lay: &Layout, y: f32) -> usize {
        match &lay.flex {
            Some(f) => f.rows.partition_point(|r| r.y <= y).saturating_sub(1),
            None => (((y - self.tuning.gap) / lay.cell_h).floor().max(0.0)) as usize,
        }
    }

    /// The clip in `row` whose center is horizontally nearest `cx` —
    /// vertical selection moves land on the visually adjacent tile even
    /// when flexible rows don't share column edges.
    fn nearest_in_row(&self, lay: &Layout, row: usize, cx: f32) -> Option<usize> {
        self.row_range(lay, row).min_by(|&a, &b| {
            let da = {
                let (x, _, w, _) = self.tile_rect(lay, a);
                (x + w * 0.5 - cx).abs()
            };
            let db = {
                let (x, _, w, _) = self.tile_rect(lay, b);
                (x + w * 0.5 - cx).abs()
            };
            da.total_cmp(&db)
        })
    }

    fn content_height(&self, lay: &Layout) -> f32 {
        match &lay.flex {
            Some(f) => f.height,
            None => self.tuning.gap + self.rows(lay) as f32 * lay.cell_h,
        }
    }

    /// Whether the row-wrap reflow animation is live this frame (flexible
    /// grid, `zoom_wrap` on, no modal, animation level with UI tweens).
    fn reflow_active(&self, lay: &Layout) -> bool {
        lay.flex.is_some()
            && self.tuning.zoom_wrap
            && !(self.quickview || self.fullview)
            && self.level().ui()
    }

    /// Advance the per-clip reflow table: seed fresh clips, glide re-justified
    /// tiles toward their new slot, and — on a frame where the layout zoom
    /// moved (`zoomed`) — start a wrap for each clip crossing a row boundary.
    /// Only visible rows are stepped (off-screen clips stay `init = false` and
    /// snap when they scroll in). Sets `motion` while any wrap or glide runs.
    fn step_reflow(&mut self, dt: f32, lay: &Layout, zoomed: bool) {
        if !self.reflow_active(lay) {
            // Drop the table so a re-enable reseeds cleanly (and so eviction
            // of the flex grid can't leave stale rects behind).
            if !self.reflow.is_empty() {
                self.reflow.clear();
            }
            return;
        }
        // Grow/shrink to match the library. New entries snap in (init =
        // false); existing tiles keep animating — an ingest arrival must not
        // reset the whole grid. Index remaps (shuffle, D swap) instead clear
        // the table at their call sites, forcing a clean reseed here.
        if self.reflow.len() != self.clips.len() {
            self.reflow.resize(self.clips.len(), TileReflow::default());
        }
        let a = alpha(self.tuning.zoom_reflow_smoothing, dt);
        let wrap_ms = self.tuning.zoom_wrap_ms.max(1.0);
        let dt_ms = dt * 1000.0;
        // Advance and retire every in-flight wrap first — visible or not. A
        // tile can wrap and then scroll off before it lands; if only visible
        // rows were ticked its `elapsed` would freeze and the wrap (and the
        // motion flag it raises) would live forever, pinning the render loop.
        let mut any_wrap = false;
        for r in &mut self.reflow {
            if let Some(w) = &mut r.wrap {
                w.elapsed += dt_ms;
                if w.elapsed >= wrap_ms {
                    r.wrap = None;
                } else {
                    any_wrap = true;
                }
            }
        }
        let (first, last) = self.visible_rows(lay, 1);
        let mut gliding = false;
        for row in first..=last {
            for i in self.row_range(lay, row) {
                let (tx, ty, tw, th) = self.tile_rect(lay, i);
                let target = [tx, ty, tw, th];
                let trow = self.row_of(lay, i) as u32;
                let Some(r) = self.reflow.get_mut(i) else {
                    continue;
                };
                if !r.init {
                    r.rect = target;
                    r.row = trow;
                    r.init = true;
                    r.wrap = None;
                } else if trow != r.row {
                    // Crossed a boundary. Only a zoom repack (this frame's
                    // layout zoom moved) wraps; a row change while settled
                    // came from an aspect/thumb arrival or a window resize —
                    // snap those (the glide absorbs shape shifts) so a
                    // loading library doesn't fling tiles around. An
                    // arrival mid-wrap must NOT kill other tiles' in-flight
                    // wraps — only this tile's own state is touched.
                    if zoomed {
                        // Anchor the exit copy where the tile was displayed;
                        // the enter slide carries the new slot in from the
                        // opposite window edge (build_frame).
                        r.wrap = Some(WrapEvent {
                            from: r.rect,
                            elapsed: 0.0,
                            forward: trow > r.row,
                        });
                    } else {
                        r.wrap = None;
                    }
                    r.row = trow;
                    r.rect = target;
                } else {
                    let mut delta: f32 = 0.0;
                    for (cur, tgt) in r.rect.iter_mut().zip(target) {
                        *cur += (tgt - *cur) * a;
                        delta = delta.max((tgt - *cur).abs());
                    }
                    // The glide is motion too: with the layout zoom snapping
                    // in wrap mode, these tweens ARE the zoom animation — if
                    // they don't hold the loop awake it freezes mid-glide.
                    if delta > 0.3 {
                        gliding = true;
                    }
                }
                if r.wrap.is_some() {
                    any_wrap = true;
                }
            }
        }
        if any_wrap || gliding {
            self.motion = true;
        }
    }

    // --- ribbon pinch (flexible grid) ---

    /// Texture uploads to accept from the media queue this frame. User
    /// attention outranks the background sweep: a playing stream squeezes
    /// it, and a pinch — which pins the loop at full rate for as long as
    /// the fingers are down — squeezes it hardest.
    fn media_upload_budget(&self, presenting: bool) -> usize {
        if self.ribbon.is_some() || self.ribbon_settle.is_some() {
            MEDIA_UPLOAD_BUDGET_GESTURE
        } else if presenting {
            MEDIA_UPLOAD_BUDGET_LIVE
        } else {
            MEDIA_UPLOAD_BUDGET
        }
    }

    /// Whether a pinch drives the gripped-ribbon model rather than the
    /// wrap reflow (flexible grid, `zoom_ribbon`, no modal, UI tweens on).
    fn ribbon_on(&self) -> bool {
        self.tuning.zoom_ribbon
            && self.tuning.grid_layout == GridStyle::Flexible
            && !(self.quickview || self.fullview)
            && self.level().ui()
            && !self.clips.is_empty()
    }

    /// Grab the strip at the cursor: the chip under it (or the nearest one
    /// in that row band), held at the nearest point ON that chip. Clamping
    /// the POINT rather than the fractions is load-bearing — clamping the
    /// fractions while keeping the raw pointer x makes grip and offset
    /// disagree, and the grip row jerks sideways on the gesture's first
    /// frame, splitting a chip that should not have moved at all.
    fn ribbon_grip(&self, lay: &Layout) -> Option<RibbonGrip> {
        let flex = lay.flex.as_ref()?;
        let (cx, cy) = self.cursor;
        let cyc = cy + self.scroll;
        let clip = self.tile_at(lay, cx, cy).or_else(|| {
            let row = self.row_at_y(lay, cyc.max(0.0));
            self.nearest_in_row(lay, row, cx)
        })?;
        let (x, y, w, h) = self.tile_rect(lay, clip);
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        let gx = cx.clamp(x, x + w);
        let gy = cyc.clamp(y, y + h);
        Some(RibbonGrip {
            clip,
            fx: (gx - x) / w,
            fy: (gy - y) / h,
            cx: gx,
            cy: gy - self.scroll,
            z0: self.zoom.max(0.01),
            grip_row: self.row_of(lay, clip),
            rest_h: flex.rows.iter().map(|r| r.h).collect(),
            last: Instant::now(),
        })
    }

    /// Lay the ribbon at `zoom`: rows walk outward from the grip row, each
    /// one joined strip at its own height, splits carried across boundaries
    /// as the same clip appearing in both rows. `all` walks the whole
    /// library (release, to quantize); otherwise it stops once the rows
    /// leave the viewport. Returns the rows and the grip row's index.
    fn ribbon_walk(&self, grip: &RibbonGrip, zoom: f32, all: bool) -> (Vec<RibbonRow>, usize) {
        let t = &self.tuning;
        let g = t.gap;
        let (edge_l, edge_r) = (g, (self.viewport.width - g).max(g + 1.0));
        let n = self.clips.len();
        let nominal = self.nominal_h(zoom);
        let k = zoom / grip.z0;
        // Rows start at their resting heights (so a gesture that hasn't
        // moved changes nothing) and blend to the ribbon's uniform height.
        let blend = ((zoom / grip.z0).ln().abs() * t.zoom_ribbon_blend).clamp(0.0, 1.0);
        let row_h = |off: i64| -> f32 {
            let ix = grip.grip_row as i64 + off;
            let base = usize::try_from(ix)
                .ok()
                .and_then(|i| grip.rest_h.get(i).copied())
                .unwrap_or(nominal / k.max(0.01));
            let scaled = base * k;
            scaled + (nominal - scaled) * blend
        };
        let on_screen = |y: f32, h: f32| y < self.viewport.height && y + h > 0.0;
        // A row can be off screen in the walk yet on screen in the settled
        // target it morphs to, so "safe to shed the middle of" needs a
        // viewport of slack on each side.
        let vh = self.viewport.height;
        let far = |y: f32, h: f32| y + h < -vh || y > 2.0 * vh;
        let visible = |y: f32, h: f32| all || on_screen(y, h);

        // The grip row: the held chip is pinned under the fingers and its
        // neighbours lay outward from it until they run past either edge.
        let h0 = row_h(0);
        let w0 = self.chip_aspect(grip.clip) * h0;
        let mut items = vec![RibbonItem {
            clip: grip.clip,
            x: grip.cx - grip.fx * w0,
            w: w0,
        }];
        let mut x = items[0].x + w0 + g;
        let mut j = grip.clip + 1;
        while j < n && x < edge_r {
            let w = self.chip_aspect(j) * h0;
            items.push(RibbonItem { clip: j, x, w });
            x += w + g;
            j += 1;
        }
        let mut xe = items[0].x;
        let mut jl = grip.clip;
        let mut left = Vec::new();
        while jl > 0 {
            jl -= 1;
            let w = self.chip_aspect(jl) * h0;
            let x2 = xe - g - w;
            if x2 + w <= edge_l {
                break;
            }
            left.push(RibbonItem { clip: jl, x: x2, w });
            xe = x2;
        }
        left.reverse();
        left.extend(items);
        let mut rows = vec![RibbonRow {
            y: grip.cy - grip.fy * h0,
            h: h0,
            items: left,
        }];
        // Rows above the grip are collected in their own list and spliced
        // on at the end (see the up-walk below) — prepending is quadratic.
        let mut above: Vec<RibbonRow> = Vec::new();

        // Downward: each row opens with the previous row's right straddler,
        // entering from the left at the same split fraction.
        for off in 1..=(n as i64) {
            let prev = rows.last().expect("grip row");
            let last = *prev.items.last().expect("non-empty row");
            let straddles = last.x + last.w > edge_r + 1e-3;
            let frac = if straddles {
                ((edge_r - last.x) / last.w).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let start = if straddles { last.clip } else { last.clip + 1 };
            if start >= n {
                break;
            }
            let h = row_h(off);
            let y = prev.y + prev.h + g;
            if !visible(y, h) {
                break;
            }
            let mut x = if straddles {
                edge_l - frac * (self.chip_aspect(start) * h)
            } else {
                edge_l
            };
            let mut items: Vec<RibbonItem> = Vec::new();
            for clip in start..n {
                if !items.is_empty() && x >= edge_r {
                    break;
                }
                let w = self.chip_aspect(clip) * h;
                items.push(RibbonItem { clip, x, w });
                x += w + g;
            }
            let done = items
                .last()
                .is_some_and(|l| l.clip + 1 >= n && l.x + l.w <= edge_r + 1e-3);
            // A row nobody can see is only read for its boundary chips
            // (quantization looks at first and last), so don't carry the
            // middle of it — a full walk of a big library is otherwise
            // one Vec per row of chips that will never be drawn.
            if far(y, h) && items.len() > 2 {
                let last = items[items.len() - 1];
                items.truncate(1);
                items.push(last);
            }
            rows.push(RibbonRow { y, h, items });
            if done {
                break;
            }
        }

        // Upward: each row closes with the next row's left straddler, its
        // hidden part showing at this row's right edge.
        for off in 1..=(n as i64) {
            // The row below this one: the last one walked up, or the grip
            // row on the first pass.
            let (head, below_y) = {
                let next = above.last().unwrap_or_else(|| rows.first().expect("grip row"));
                (next.items[0], next.y)
            };
            let straddles = head.x < edge_l - 1e-3;
            let hidden = if straddles {
                ((edge_l - head.x) / head.w).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let end = if straddles {
                head.clip
            } else if head.clip == 0 {
                break;
            } else {
                head.clip - 1
            };
            let h = row_h(-off);
            let y = below_y - h - g;
            if !visible(y, h) {
                break;
            }
            let w = self.chip_aspect(end) * h;
            // Laid right to left, then reversed — prepending each chip (and
            // each row) into a growing Vec is quadratic, which on a large
            // library froze the main thread for seconds at every release.
            let mut items = vec![RibbonItem {
                clip: end,
                x: if straddles {
                    edge_r - hidden * w
                } else {
                    edge_r - w
                },
                w,
            }];
            let mut xe = items[0].x;
            let mut clip = end;
            while clip > 0 {
                clip -= 1;
                let w = self.chip_aspect(clip) * h;
                let x2 = xe - g - w;
                if x2 + w <= edge_l {
                    break;
                }
                items.push(RibbonItem { clip, x: x2, w });
                xe = x2;
            }
            items.reverse();
            let done = items[0].clip == 0 && items[0].x >= edge_l - 1e-3;
            if far(y, h) && items.len() > 2 {
                let last = items[items.len() - 1];
                items.truncate(1);
                items.push(last);
            }
            above.push(RibbonRow { y, h, items });
            if done {
                break;
            }
        }
        let grip_ix = above.len();
        above.reverse();
        above.extend(rows);
        (above, grip_ix)
    }

    /// How much a row of `start..end` must scale from nominal to span the
    /// width — the flexible grid's row scale, shared by the packing rebuild
    /// and by quantization so the two can't disagree about what fits.
    fn ribbon_row_scale(&self, start: usize, end: usize, zoom: f32) -> f32 {
        let g = self.tuning.gap;
        let cnt = (end - start) as f32;
        let sum: f32 = (start..end).map(|i| self.chip_aspect(i)).sum();
        let usable = (self.viewport.width - g * (cnt + 1.0)).max(1.0);
        usable / sum.max(0.01) / self.nominal_h(zoom)
    }

    /// Quantize a walked ribbon to the nearest SAFE packing: every chip
    /// straddling a row boundary joins whichever row shows more of it —
    /// unless keeping it would squeeze that row past `row_height_min`, in
    /// which case it goes down (the direction that always fits). The
    /// result is row START indices, which makes the packing a partition by
    /// construction: no chip can land in two rows, and nothing is dragged
    /// sideways to left-align a row the gesture left indented.
    fn ribbon_quantize(&self, rows: &[RibbonRow], zoom: f32) -> Vec<usize> {
        let n = self.clips.len();
        let edge_r = (self.viewport.width - self.tuning.gap).max(1.0);
        let rmin = self.tuning.row_height_min.clamp(0.1, 1.0);
        let mut starts = vec![0usize];
        let mut cur = 0usize;
        for (i, row) in rows.iter().enumerate() {
            let Some(next) = rows.get(i + 1) else { break };
            let last = *row.items.last().expect("non-empty row");
            let shared = next.items.first().map(|it| it.clip) == Some(last.clip);
            let keep_up = !shared
                || ((edge_r - last.x) / last.w >= 0.5
                    && self.ribbon_row_scale(cur, last.clip + 1, zoom) >= rmin);
            let start = if keep_up { last.clip + 1 } else { last.clip };
            // Rows must stay a strictly ascending partition, and the tail
            // of the library has to remain reachable.
            let start = start.clamp(cur + 1, n.saturating_sub(1));
            if start <= cur {
                break;
            }
            starts.push(start);
            cur = start;
        }
        starts
    }

    /// Rebuild a packing's geometry at `zoom`. Full rows justify (their
    /// height scaled so members exactly span the width); the strip's first
    /// and last rows keep their slack when justifying would stretch them —
    /// the first row right-aligned (an indentation on the left is fine),
    /// the last row left-aligned.
    ///
    /// The starts are a SEED, not a contract: a thumb landing changes a
    /// clip's aspect, and a row that no longer fits simply hands its
    /// trailing chip down to the next row (the direction that always
    /// fits) rather than invalidating the whole packing. Dropping the
    /// packing instead re-justified the entire grid on the next arrival —
    /// which read as the settled grid snapping chip 0 back to the left
    /// margin a moment after it came to rest.
    fn build_ribbon_flex(&self, starts: &[usize], zoom: f32) -> Option<FlexGrid> {
        let t = &self.tuning;
        let g = t.gap;
        let vw = self.viewport.width;
        let n = self.clips.len();
        if starts.first() != Some(&0) || starts.last().is_some_and(|&s| s >= n) {
            return None;
        }
        let nominal = self.nominal_h(zoom);
        let rmin = t.row_height_min.clamp(0.1, 1.0);
        let rmax = t.row_height_max.clamp(1.0, 4.0);
        let mut rows = Vec::with_capacity(starts.len());
        let mut y = g;
        let mut start = 0usize;
        let mut seed = 1usize;
        while start < n {
            let seed_end = starts.get(seed).copied().unwrap_or(n);
            let mut end = seed_end.clamp(start + 1, n);
            // Heal an overfull row by handing chips down, one at a time.
            while end > start + 1 && self.ribbon_row_scale(start, end, zoom) < rmin {
                end -= 1;
            }
            if end >= seed_end {
                seed += 1;
            }
            let r = rows.len();
            let last_row = end >= n;
            let scale = self.ribbon_row_scale(start, end, zoom);
            let (h, align_right) = if scale <= 1.0 {
                (nominal * scale.max(rmin), false)
            } else if last_row {
                (nominal, false) // never grows; slack on the right
            } else if r == 0 && scale >= 1.35 {
                (nominal, true) // keeps its indentation on the left
            } else {
                (nominal * scale.min(rmax), false)
            };
            let mut x = Vec::with_capacity(end - start);
            let mut w = Vec::with_capacity(end - start);
            let mut cx = g;
            for i in start..end {
                let tw = self.chip_aspect(i) * h;
                x.push(cx);
                w.push(tw);
                cx += tw + g;
            }
            if align_right {
                let shift = (vw - g - (cx - g)).max(0.0);
                for v in &mut x {
                    *v += shift;
                }
            }
            rows.push(FlexRow {
                start,
                end,
                y,
                h,
                x,
                w,
            });
            y += h + g;
            start = end;
        }
        Some(FlexGrid { rows, height: y })
    }

    /// End of gesture: quantize what the ribbon is showing, then morph
    /// every row onto its resting slot. The target is built from the SAME
    /// geometry the layout will use afterwards, so the handoff at the end
    /// of the morph is exact — no snap.
    fn ribbon_release(&mut self) {
        let Some(grip) = self.ribbon.take() else {
            return;
        };
        let zoom = self.zoom;
        let (from, grip_ix) = self.ribbon_walk(&grip, zoom, true);
        let starts = self.ribbon_quantize(&from, zoom);
        // A collapsed row would break the row-to-row correspondence the
        // morph relies on; install the packing directly in that rare case.
        let Some(grid) = self
            .build_ribbon_flex(&starts, zoom)
            .filter(|g| g.rows.len() == from.len())
        else {
            self.ribbon_install(starts, zoom, None);
            return;
        };
        // Keep the grip row where it sits: that fixes the scroll.
        let scroll = (grid.rows[grip_ix].y - from[grip_ix].y)
            .clamp(0.0, (grid.height - self.viewport.height).max(0.0));
        let g = self.tuning.gap;
        // Only the rows the morph will actually draw get built: it never
        // shows anything off screen, and materialising a second copy of
        // the whole library per gesture is exactly the kind of churn that
        // made a long session degrade.
        let vw = self.viewport.width;
        let vh = self.viewport.height;
        let keep: Vec<usize> = (0..from.len())
            .filter(|&r| {
                let (a, b) = (&from[r], &grid.rows[r]);
                let (top, bot) = (
                    a.y.min(b.y - scroll),
                    (a.y + a.h).max(b.y - scroll + b.h),
                );
                top < vh && bot > 0.0
            })
            .collect();
        let to: Vec<RibbonRow> = keep
            .iter()
            .map(|&r| {
                let (wrow, grow) = (&from[r], &grid.rows[r]);
                let items = wrow
                    .items
                    .iter()
                    .map(|it| {
                        let w = self.chip_aspect(it.clip) * grow.h;
                        // A chip that left this row keeps a copy continuing
                        // the strip, so shedding it never has to break the
                        // row's joins — it just slides out. It has to leave
                        // through the WINDOW edge, not merely one gap past
                        // the row: a row resting indented (or short) would
                        // otherwise strand the departing copy in view, and
                        // installing the packing popped it away — the snap
                        // that put chip 0 back against the left margin.
                        // Clamped exactly to the edge, so a row resting hard
                        // against the margin keeps the strip rigid (the
                        // clamp is a no-op there) and only an indented or
                        // short row lets the gap open — which is that row's
                        // slack appearing behind the departing chip.
                        let x = if it.clip < grow.start {
                            (grow.x[0] - g - w).min(-w)
                        } else if it.clip >= grow.end {
                            let k = grow.x.len() - 1;
                            (grow.x[k] + grow.w[k] + g).max(vw)
                        } else {
                            grow.x[it.clip - grow.start]
                        };
                        RibbonItem { clip: it.clip, x, w }
                    })
                    .collect();
                RibbonRow {
                    y: grow.y - scroll,
                    h: grow.h,
                    items,
                }
            })
            .collect();
        let mut from = from;
        let mut kept = keep.iter().copied().peekable();
        let mut r = 0usize;
        from.retain(|_| {
            let hit = kept.peek() == Some(&r);
            if hit {
                kept.next();
            }
            r += 1;
            hit
        });
        self.ribbon_settle = Some(RibbonSettle {
            from,
            to,
            t: 0.0,
            pack: RibbonPack {
                starts,
                vw: self.viewport.width,
                zoom,
            },
            scroll,
        });
        self.motion = true;
    }

    /// Adopt a packing as the resting layout (and the scroll that keeps the
    /// view where the gesture left it).
    fn ribbon_install(&mut self, starts: Vec<usize>, zoom: f32, scroll: Option<f32>) {
        self.ribbon_pack = Some(RibbonPack {
            starts,
            vw: self.viewport.width,
            zoom,
        });
        self.flex_cache.borrow_mut().take();
        if let Some(s) = scroll {
            self.scroll = s;
            self.scroll_target = s;
        }
        let lay = self.layout();
        let max = self.max_scroll(&lay);
        self.scroll = self.scroll.clamp(0.0, max);
        self.scroll_target = self.scroll_target.clamp(0.0, max);
        // The wrap table's rects belong to the pre-gesture layout.
        self.reflow.clear();
    }

    /// Drop every ribbon state — a keyboard zoom, resize or index remap
    /// returns the grid to its plain justified packing.
    fn ribbon_reset(&mut self) {
        let had = self.ribbon_pack.is_some();
        self.ribbon = None;
        self.ribbon_settle = None;
        self.ribbon_pack = None;
        if had {
            self.flex_cache.borrow_mut().take();
        }
    }

    /// Rows to draw this frame while a ribbon gesture or its settle is up.
    fn ribbon_rows(&self) -> Option<Vec<RibbonRow>> {
        if let Some(s) = &self.ribbon_settle {
            let t = s.t.clamp(0.0, 1.0);
            let lerp = |a: f32, b: f32| a + (b - a) * t;
            return Some(
                s.from
                    .iter()
                    .zip(&s.to)
                    .map(|(f, to)| RibbonRow {
                        y: lerp(f.y, to.y),
                        h: lerp(f.h, to.h),
                        items: f
                            .items
                            .iter()
                            .zip(&to.items)
                            .map(|(a, b)| RibbonItem {
                                clip: a.clip,
                                x: lerp(a.x, b.x),
                                w: lerp(a.w, b.w),
                            })
                            .collect(),
                    })
                    .collect(),
            );
        }
        let grip = self.ribbon.as_ref()?;
        Some(self.ribbon_walk(grip, self.zoom, false).0)
    }

    /// Advance the release morph and end a gesture that has gone quiet.
    fn step_ribbon(&mut self, dt: f32) {
        if let Some(grip) = &self.ribbon {
            let quiet = grip.last.elapsed().as_secs_f32() * 1000.0;
            if quiet >= self.tuning.zoom_ribbon_release_ms.max(1.0) {
                self.ribbon_release();
            } else {
                self.motion = true;
            }
        }
        if let Some(s) = &mut self.ribbon_settle {
            s.t += (1.0 - s.t) * alpha(self.tuning.zoom_ribbon_settle, dt);
            if s.t >= 0.995 {
                let s = self.ribbon_settle.take().expect("settle in flight");
                self.ribbon_install(s.pack.starts, s.pack.zoom, Some(s.scroll));
            } else {
                self.motion = true;
            }
        }
    }

    fn max_scroll(&self, lay: &Layout) -> f32 {
        (self.content_height(lay) - self.viewport.height).max(0.0)
    }

    fn tile_at(&self, lay: &Layout, x: f32, y: f32) -> Option<usize> {
        let yy = y + self.scroll;
        if yy < 0.0 {
            return None;
        }
        let row = self.row_at_y(lay, yy);
        self.row_range(lay, row).find(|&i| {
            let (tx, ty, tw, th) = self.tile_rect(lay, i);
            x >= tx && x <= tx + tw && yy >= ty && yy <= ty + th
        })
    }

    // --- selection ---

    /// The attention-lane interaction model is on (DESIGN.md §15 spike).
    fn attention(&self) -> bool {
        self.tuning.interaction == Interaction::Attention
    }

    /// Which clip the hires lane should play. Classic: always the
    /// selection. Attention (grid only — modals always show the
    /// selection): the hovered tile while mousing, the selection while
    /// keyboard-navigating. Strict rule: mousing over empty space means
    /// nothing plays (no "last selected keeps playing" fallback yet).
    fn attention_target(&self) -> Option<usize> {
        if self.attention() && !self.quickview && !self.fullview && self.mouse_attention {
            self.hovered
        } else {
            Some(self.selected)
        }
    }

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
            // Vertical moves land on the horizontally nearest tile in the
            // adjacent row — the same-column tile in the fixed grid, the
            // visually adjacent one under flexible rows.
            let rows = self.rows(&lay) as i32;
            let row = ((self.row_of(&lay, self.selected) as i32) + dy).clamp(0, rows - 1) as usize;
            let (x, _, w, _) = self.tile_rect(&lay, self.selected);
            self.nearest_in_row(&lay, row, x + w * 0.5)
                .unwrap_or(self.selected)
        };
        if idx != self.selected {
            self.selected = idx;
            self.sel_changed_at = Instant::now();
            // An explicit move outranks the D swap's pending reselect.
            self.pending_reselect = None;
        }
        // A keyboard move hands the attention lane to the selection.
        self.mouse_attention = false;
        self.scroll_to_selected();
    }

    /// Smoothly bring the selected row toward the vertical center. Uses the
    /// gentler key-move chase curve so whole-screen jumps glide, not jolt.
    fn scroll_to_selected(&mut self) {
        let lay = self.layout();
        let (_, ty, _, th) =
            self.tile_rect(&lay, self.selected.min(self.clips.len().saturating_sub(1)));
        let row_center = ty + th * 0.5;
        self.scroll_target =
            (row_center - self.viewport.height * 0.5).clamp(0.0, self.max_scroll(&lay));
        self.scroll_vel = 0.0;
        self.chase = self.tuning.key_snap_strength;
    }

    /// Move the selection to an arbitrary index (random jump, auto-skip
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
        // Programmatic moves (random jump, auto-skip) are selection acts:
        // the attention lane follows the selection, not a stale hover.
        self.mouse_attention = false;
        self.scroll_to_selected();
        if self.quickview && (idx as f32 - self.strip_pos).abs() > 4.0 {
            self.strip_pos = idx as f32;
            self.strip_target = self.strip_pos;
        }
        // Not always an input event (auto-skip calls this): keep the
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

    /// Remap every index-keyed reference after `self.clips` has been
    /// rebuilt in a new order (shuffle, sorted-ingest merge, unplayable
    /// removal). `new_of_old[old] = Some(new)`, `None` = the clip is gone:
    /// its index-map entry drops, its atlas slots free, its live lanes
    /// die, and the selection falls to the nearest surviving neighbor.
    /// Surviving clips' reflow entries move WITH them, so tiles glide to
    /// their new slots instead of snapping (the sorted-ingest "animate
    /// into place"); callers that want a crossfade instead (shuffle)
    /// clear `reflow` themselves afterwards.
    fn remap_refs(&mut self, new_of_old: &[Option<usize>]) {
        let m = |i: usize| new_of_old.get(i).copied().flatten();
        self.index.retain(|_, v| match m(*v) {
            Some(n) => {
                *v = n;
                true
            }
            None => false,
        });
        for slot in self.slots.iter_mut() {
            if let Some((owner, _)) = slot {
                match m(*owner) {
                    Some(n) => *owner = n,
                    None => *slot = None,
                }
            }
        }
        if let Some(mut l) = self.live_sel.take() {
            if let Some(n) = m(l.clip) {
                l.clip = n;
                self.live_sel = Some(l);
            }
        }
        if let Some(mut h) = self.live_hover.take() {
            if let Some(n) = m(h.clip) {
                h.clip = n;
                self.live_hover = Some(h);
            }
        }
        self.warm.retain_mut(|w| match m(w.clip) {
            Some(n) => {
                w.clip = n;
                true
            }
            None => false,
        });
        self.hover_resume = self.hover_resume.and_then(|(i, p)| m(i).map(|n| (n, p)));
        self.hovered = self.hovered.and_then(m);
        self.strip_hover = self.strip_hover.and_then(m);
        let old_sel = self.selected;
        self.selected = m(old_sel).unwrap_or_else(|| {
            // Removed from under the selection: nearest surviving neighbor.
            for d in 1..=new_of_old.len() {
                if let Some(n) = old_sel.checked_sub(d).and_then(m) {
                    return n;
                }
                if let Some(n) = m(old_sel + d) {
                    return n;
                }
            }
            0
        });
        self.selected = self.selected.min(self.clips.len().saturating_sub(1));
        // A vanished selection is a real selection change; a pure
        // renumbering isn't (the same clip keeps playing).
        if m(old_sel).is_none() {
            self.sel_changed_at = Instant::now();
        }
        self.marked = self.marked.iter().filter_map(|&i| m(i)).collect();
        let old_reflow = std::mem::take(&mut self.reflow);
        self.reflow = vec![TileReflow::default(); self.clips.len()];
        for (o, r) in old_reflow.into_iter().enumerate() {
            if let Some(n) = m(o)
                && let Some(dst) = self.reflow.get_mut(n)
            {
                *dst = r;
            }
        }
        self.ribbon_reset(); // the ribbon packing is index-keyed too
        self.grid_rev = self.grid_rev.wrapping_add(1);
        if self.quickview {
            self.strip_pos = self.selected as f32;
            self.strip_target = self.strip_pos;
        }
    }

    /// The gatekeeper's second stage: a clip that passed the header sniff
    /// but failed real thumbnail extraction (truncated download, corrupt
    /// container, vanished file) leaves the grid — neighbors glide in
    /// over the hole via the permuted reflow table.
    fn remove_clip(&mut self, i: usize) {
        if i >= self.clips.len() {
            return;
        }
        let n = self.clips.len();
        self.clips.remove(i);
        let new_of_old: Vec<Option<usize>> = (0..n)
            .map(|o| match o.cmp(&i) {
                std::cmp::Ordering::Less => Some(o),
                std::cmp::Ordering::Equal => None,
                std::cmp::Ordering::Greater => Some(o - 1),
            })
            .collect();
        self.remap_refs(&new_of_old);
        self.wake(0.6);
    }

    /// `shuffle_library`: Fisher–Yates the cached clips to the front of
    /// the grid (uncached ones keep their order after them). All per-clip
    /// state rides inside `Clip` and moves with it; everything keyed by
    /// clip *index* — the path→index map, atlas slot owners, the live/
    /// warm/hover lanes, the selection — is remapped through the
    /// permutation, so the selected clip keeps playing and lands
    /// somewhere new. Hover clears (the pointer is over a different clip
    /// now); mid-ingest arrivals simply append after the shuffled block
    /// (a shuffle disarms sorted ingest — the arrangement is the
    /// shuffle's now).
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
        let map: Vec<Option<usize>> = new_of_old.into_iter().map(Some).collect();
        self.remap_refs(&map);
        // reflow[i] named a different clip — drop it so the wrap table
        // reseeds cleanly (the shuffle rides the crossfade, not a glide
        // across the whole rearrangement).
        self.reflow.clear();
        self.hovered = None;
        self.strip_hover = None;
        self.hover_resume = None; // hover clears anyway
        // The arrangement is the shuffle's now — later arrivals append.
        self.sort_armed = false;
        // Crossfade the old arrangement out, like the zoom reflow / D swap.
        if !self.last_tiles.is_empty() {
            self.transition = Some((std::mem::take(&mut self.last_tiles), Instant::now()));
        }
        self.scroll_to_selected();
        self.wake(1.0);
    }

    /// Auto-skip: with the timer armed AND a modal view (quickview or
    /// fullview) up, advance to the next clip (wrapping) once the current
    /// one has played `auto_skip_s`. "Played" means live frames on screen
    /// — the countdown starts at first frame, not at selection, so
    /// cold-spawn latency doesn't eat watch time. At levels without live
    /// video it counts from the selection change; a stream that never
    /// delivers gets `AUTO_SKIP_SPAWN_GRACE_S` before the slideshow moves
    /// on anyway. Whenever the countdown is suspended — back in the grid,
    /// paused, parked, scrubbing, mid-swap, chapter bar up — the anchor
    /// refreshes, so clearing the suspension restarts a fresh countdown
    /// (reopening a modal never inherits stale watch time).
    fn tick_auto_skip(&mut self) {
        if self.auto_skip_since.is_none() {
            return;
        }
        let suspended = !(self.quickview || self.fullview)
            || self.clips.is_empty()
            || self.paused()
            || self.sel_parked
            || self.scrubbing
            || self.pending_reselect.is_some()
            || self.chapters.is_some();
        if suspended {
            self.auto_skip_since = Some(Instant::now());
            return;
        }
        if self.auto_skip_progress() >= Some(1.0) {
            let next = (self.selected + 1) % self.clips.len();
            if next != self.selected {
                self.select_index(next);
            }
        }
    }

    /// Elapsed fraction (0..1) of the auto-skip countdown — `Some` only
    /// while the timer is armed and a modal view is up, which doubles as
    /// the timer ring's visibility gate. 1.0 = time to advance.
    fn auto_skip_progress(&self) -> Option<f32> {
        let since = self.auto_skip_since?;
        if !(self.quickview || self.fullview) || self.clips.is_empty() {
            return None;
        }
        let limit = self.tuning.auto_skip_s.max(0.05);
        let first_frame = self
            .live_sel
            .as_ref()
            .filter(|l| l.clip == self.selected)
            .and_then(|l| l.first_frame);
        let start = match first_frame {
            Some(ff) => ff.max(since),
            None if !self.level().live() => self.sel_changed_at.max(since),
            // Live expected but nothing delivered yet: bill the stall
            // grace up front so the ring honestly shows "not started".
            None => {
                self.sel_changed_at.max(since) + Duration::from_secs_f32(AUTO_SKIP_SPAWN_GRACE_S)
            }
        };
        let elapsed = Instant::now().saturating_duration_since(start);
        Some((elapsed.as_secs_f32() / limit).min(1.0))
    }

    // --- input ---

    fn key(&mut self, key: Key) {
        // Esc is the only hardwired key; everything else — movement
        // included (the context-sensitive `move_*` actions on hjkl and
        // the arrows by default) — goes through the keymap.
        match key {
            // Esc peels layers: a deliberately-opened chapter bar slides
            // down first, then fullview exits, then quickview. A transient
            // peek (from a chapter step) doesn't count as a layer — Esc
            // exits fullview and the peek drops with it.
            Key::Escape if self.chapters.as_ref().is_some_and(|b| b.open && b.peek_until.is_none()) => {
                return self.close_chapter_bar();
            }
            Key::Escape if self.fullview => {
                self.chapters = None; // drop any lingering peek
                self.fullview = false;
                return;
            }
            Key::Escape if self.quickview => {
                self.quickview = false;
                return;
            }
            // In the grid, Esc drops the multi-select marks (attention
            // mode's cmd/shift-click) — the last layer to peel.
            Key::Escape if !self.marked.is_empty() => {
                self.marked.clear();
                self.wake(0.3);
                return;
            }
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
            Action::SelectNext => self.move_selection(1, 0),
            Action::SelectPrev => self.move_selection(-1, 0),
            // Context-sensitive movement (the default hjkl/arrows).
            // (move_* are single-axis, so dx and dy are never both
            // nonzero.) The vertical axis is a view-depth ladder between
            // the modals, with dead-ends at each end:
            //   fullview:  left/right step chapters (peeking the bar),
            //              down → filmstrip quickview, up → dismiss the
            //              chapter bar if it's up, else nothing (ceiling)
            //   quickview: left/right move the selection along the strip,
            //              up → fullview, down → back to the grid
            //   grid:      every direction moves the selection.
            Action::Move { dx, dy } => {
                if self.fullview {
                    if dx != 0 {
                        self.chapter_step(dx > 0);
                    } else if dy < 0 {
                        // Up: swipe the chapter bar back down off screen
                        // if it's up; otherwise nothing (the ceiling).
                        if self.chapters.as_ref().is_some_and(|b| b.open) {
                            self.close_chapter_bar();
                        }
                    } else if dy > 0 {
                        self.to_quickview();
                    }
                } else if self.quickview {
                    if dy < 0 {
                        self.fullview = true;
                    } else if dy > 0 {
                        self.quickview = false; // down drops back to the grid
                    } else {
                        self.move_selection(dx, dy);
                    }
                } else {
                    self.move_selection(dx, dy);
                }
            }
            Action::OpenParent => self.open_parent(),
            Action::OpenLibrary { dir } => self.open_library(dir),
            Action::JumpRandom => self.jump_random(),
            Action::ShuffleLibrary => self.shuffle_library(),
            Action::ToggleAutoSkip => {
                self.auto_skip_since = match self.auto_skip_since {
                    Some(_) => None,
                    None => Some(Instant::now()),
                };
                log::info!(
                    "auto-skip {} ({}s per clip, runs in quickview/fullview)",
                    if self.auto_skip_since.is_some() {
                        "on"
                    } else {
                        "off"
                    },
                    self.tuning.auto_skip_s
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
    /// in-place `seek()` on the resident decoder (DESIGN.md §15). The
    /// demuxer jumps and the decoder flushes; the last frame stays on
    /// screen (the hires texture still holds it) until the new position
    /// delivers, so it reads as freeze-then-jump — GOP-bound (~30–600ms)
    /// instead of the old ~1s respawn floor, and chained presses need no
    /// checkpoint machinery. Keyframe seeks: the landing keyframe shows
    /// instantly, no GOP decode-forward freeze (mpv-style interactive
    /// seeking) — the small drift off the exact fraction is invisible for
    /// a skip. Wraps at the ends — playback loops anyway.
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
        l.player.seek((l.position() + delta).rem_euclid(d), false);
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

    /// How far the seekbar rides up off the video's bottom edge. Zero in
    /// quickview; in fullview it lifts with the chapter bar's slide so the
    /// bar stays visible above the chips instead of fading out under them.
    /// Centralized here so the draw AND every hit-test (hover storyboard,
    /// click, scrub) share the lifted position — otherwise the hover thumb
    /// tracks the old bottom-edge band while the bar is drawn higher.
    fn seekbar_lift(&self) -> f32 {
        if self.fullview {
            let slide = self.chapters.as_ref().map(|b| b.slide).unwrap_or(0.0);
            if slide > 0.0 {
                let (_, chh, _) = self.strip_geom();
                return slide * (chh + 34.0);
            }
        }
        0.0
    }

    /// Seekbar line geometry: (left x, width, bottom y). The bar floats
    /// inset from the video's edges (side padding scales with the frame),
    /// shared by the quickview modal and fullview. `bottom y` already
    /// includes `seekbar_lift`.
    fn seekbar_line(&self) -> Option<(f32, f32, f32)> {
        let (x, y, w, h) = self.active_video_rect()?;
        let pad_x = (w * 0.05).clamp(16.0, 48.0);
        let pad_y = (h * 0.05).clamp(14.0, 32.0);
        Some((x + pad_x, (w - pad_x * 2.0).max(8.0), y + h - pad_y - self.seekbar_lift()))
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
        // `bot` already includes seekbar_lift (fullview raises the bar above
        // the chapter bar), so the hover storyboard/click/scrub bands match.
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
            pie: 0.0,
        };
        // Scrim first: the same soft dark backing the auto-skip ring wears,
        // so the white track/fill stay legible over bright video.
        tiles.push(Tile {
            x: bx - SEEKBAR_SCRIM_PAD,
            y: bot - bh - SEEKBAR_SCRIM_PAD,
            w: bw + SEEKBAR_SCRIM_PAD * 2.0,
            h: bh + SEEKBAR_SCRIM_PAD * 2.0,
            color: [0.0, 0.0, 0.0, SCRIM_ALPHA * bar_a],
            border_color: [0.0; 4],
            corner_radius: bh * 0.5 + SEEKBAR_SCRIM_PAD,
            border_width: 0.0,
            uv: [0.0; 4],
            uv2: [0.0; 4],
            frame_fade: 0.0,
            tex_mix: 0.0,
            hires: false,
            pie: 0.0,
        });
        // Track: a translucent-white tint (reads as a bright frosted line
        // over the video, not a grey slab); the fill is solid white.
        let track_a = t.seekbar_track_opacity.clamp(0.0, 1.0);
        tiles.push(bar(bx, bw, [1.0, 1.0, 1.0, track_a * bar_a]));
        tiles.push(bar(bx, (bw * pos).max(bh), [1.0, 1.0, 1.0, 0.95 * bar_a]));
        // Storyboard preview (DESIGN.md §14 M8 phase 1): the anim sheet is
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
                    pie: 0.0,
                });
            }
        }
    }

    /// Seek the selected clip's stream to a fraction of its duration.
    /// Keyframe mode while a drag is in flight (instant feedback), exact
    /// on release / click-settle (DESIGN.md §14 M8 two-phase scrub).
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
        // Siblings: this directory's own videos, non-recursive.
        let rx = ingest::spawn_dir_reader(dir.to_path_buf(), self.tuning.gatekeeper, self.notify.clone());
        self.swap_library(rx, Some(path));
    }

    /// `open_library` (a `[keys]` binding with an inline path): swap the
    /// whole library to an explicit directory, streamed like a CLI path arg
    /// — so it honours the ingest `recurse` flag (a real library dir usually
    /// has subfolders, unlike the flat siblings view). No clip to preserve,
    /// so the first streamed clip becomes the selection.
    fn open_library(&mut self, dir: PathBuf) {
        if self.demo || self.pending_reselect.is_some() {
            return;
        }
        log::info!("opening library {}", dir.display());
        let rx = ingest::spawn_args_reader(
            vec![dir],
            self.tuning.recurse,
            self.tuning.gatekeeper,
            self.notify.clone(),
        );
        self.swap_library(rx, None);
    }

    /// Tear down all index-keyed per-clip state and stream a fresh listing
    /// in from `rx` (shared by `open_parent` and `open_library`). `reselect`
    /// shields a clip's live stream across the churn until its path streams
    /// back in (siblings view); `None` starts clean at the first clip.
    fn swap_library(
        &mut self,
        rx: Receiver<ingest::Ingested>,
        reselect: Option<PathBuf>,
    ) {
        self.rx = Some(rx);
        // All per-clip state is index-keyed; drop it and let the new
        // listing stream in (thumbs re-serve from the disk cache).
        self.chapters = None; // the bar's clip context is gone
        self.clips.clear();
        self.reflow.clear(); // index-keyed wrap state; the new listing restreams
        self.ribbon_reset(); // ditto for the ribbon packing
        self.grid_rev = self.grid_rev.wrapping_add(1);
        self.index.clear();
        self.slots.fill(None);
        self.live_hover = None;
        self.warm.clear();
        self.hovered = None;
        self.strip_hover = None;
        self.hover_resume = None; // index-keyed; the indices are gone
        self.marked.clear(); // index-keyed; the indices are gone
        self.selected = 0;
        self.pending_reselect = reselect;
        // A fresh library: sorted ingest re-arms after any shuffle.
        self.sort_armed = self.sort != SortMode::None;
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
        self.open_chapter_bar(false);
    }

    /// Bring the chapter bar up over fullview. `peek = false` is the
    /// deliberate `g` open — it stays until dismissed; `peek = true` is a
    /// transient reveal (a fullview chapter step) that slides back down
    /// after `chapter_peek_s`, the timer refreshed on each step. Re-opening
    /// a bar that's already up refreshes the peek window but never
    /// downgrades a sticky (g-opened) bar to a peek.
    fn open_chapter_bar(&mut self, peek: bool) {
        let path = match self.clips.get(self.selected) {
            Some(c) if !self.demo && c.readable && !c.cloud => c.path.clone(),
            _ => return,
        };
        self.fullview = true;
        let peek_until = (peek && self.tuning.chapter_peek_s > 0.0)
            .then(|| Instant::now() + Duration::from_secs_f32(self.tuning.chapter_peek_s));
        // Already showing this clip's bar: just refresh, don't rebuild
        // (rebuilding would re-fire the probe and reset the strip pan).
        if let Some(b) = self.chapters.as_mut().filter(|b| b.path == path) {
            b.open = true;
            if peek {
                // Refresh a peek's timer; a sticky bar stays sticky.
                if b.peek_until.is_some() {
                    b.peek_until = peek_until;
                }
            } else {
                b.peek_until = None;
            }
            self.wake(0.8);
            return;
        }
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
            peek_until,
            nav: None,
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

    /// Step out of fullview to the filmstrip quickview — the `move_up`
    /// key, the mirror of clicking the quickview video to dive in. Drops
    /// any chapter bar, and if fullview was entered directly (no quickview
    /// underneath) brings the strip up centered on the selection.
    fn to_quickview(&mut self) {
        self.chapters = None;
        self.fullview = false;
        if !self.quickview {
            self.quickview = true;
            self.quickview_at = Instant::now();
            self.strip_pos = self.selected as f32;
            self.strip_target = self.strip_pos;
        }
        self.wake(0.3);
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
            d.map(synth_checkpoints).unwrap_or_default()
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
    /// timestamp — a keyframe in-place seek (lands instantly on the
    /// nearest keyframe, no decode-forward freeze), flashing the position
    /// bar like a `[`/`]` skip.
    fn chapter_seek(&mut self, i: usize) {
        let Some(b) = &self.chapters else { return };
        let Some(&t) = b.times.as_ref().and_then(|ts| ts.get(i)) else {
            return;
        };
        let Some(l) = &self.live_sel else { return };
        if l.path != b.path {
            return;
        }
        // Stay a frame short of the end, like scrub_seek: a seek to the
        // tail lands on EOF and loops back to 0:00.
        let target = match l.duration.filter(|d| *d > 0.05) {
            Some(d) => t.clamp(0.0, (d - 0.05).max(0.0)),
            None => t.max(0.0),
        };
        l.player.seek(target, false);
        // Pin the clicked chapter so the highlight lands on it despite the
        // keyframe seek undershooting the boundary (see `resolve_chapter`).
        if let Some(b) = self.chapters.as_mut() {
            b.nav = Some(i);
        }
        self.skip_flash_at = Some(Instant::now());
        self.wake(1.5);
    }

    /// The chapter the stream is currently inside, for the bar's highlight
    /// and the step base — the position-derived index (last start ≤ pos)
    /// reconciled with the navigation intent (`ChapterBar::nav`).
    fn current_chapter(&self, bar: &ChapterBar) -> Option<usize> {
        let times = bar.times.as_ref().filter(|ts| !ts.is_empty())?;
        let l = self.live_sel.as_ref().filter(|l| l.path == bar.path)?;
        let p = match bar.duration.or(l.duration).filter(|d| *d > 0.05) {
            Some(d) => l.position().rem_euclid(d),
            None => l.position(),
        };
        let pos_idx = times.partition_point(|&t0| t0 <= p).saturating_sub(1);
        Some(resolve_chapter(pos_idx, bar.nav))
    }

    /// The clip's chapter plan whether or not the bar is up: an open
    /// bar's resolved plan wins, else the probe cache (fullview pre-warms
    /// it on entry) — real chapters, or checkpoints synthesized from the
    /// duration exactly like `apply_chapter_plan`. `None` while nothing
    /// has answered yet, or when the plan is legitimately empty (a
    /// chapterless clip under a minute).
    fn chapter_starts(&self, path: &std::path::Path) -> Option<(Vec<f64>, Option<f64>)> {
        if let Some(b) = self.chapters.as_ref().filter(|b| b.path == path)
            && let Some(ts) = &b.times
        {
            return Some((ts.clone(), b.duration));
        }
        let (times, d) = self.chapter_probe.get(path)?.as_ref()?;
        let d = d
            .or_else(|| {
                self.live_sel
                    .as_ref()
                    .filter(|l| l.path == *path)
                    .and_then(|l| l.duration)
            })
            .filter(|d| *d > 0.05);
        let times = if times.len() >= 2 {
            times.clone()
        } else {
            d.map(synth_checkpoints).unwrap_or_default()
        };
        (!times.is_empty()).then_some((times, d))
    }

    /// `move_left`/`move_right` in fullview (chapter bar up or not):
    /// step the playing stream between chapter starts — the same plan
    /// the bar shows. Forward jumps to the next start (wrapping past the
    /// last); back restarts the current chapter when more than
    /// `CHAPTER_RESTART_S` into it, else steps to the previous one
    /// (wrapping before the first). With no plan available (probe still
    /// in flight, or a chapterless clip under a minute) it falls back to
    /// a plain `skip_fraction` skip so the key always lands somewhere.
    fn chapter_step(&mut self, forward: bool) {
        let Some(path) = self.clips.get(self.selected).map(|c| c.path.clone()) else {
            return;
        };
        let Some((times, d)) = self.chapter_starts(&path) else {
            return self.skip(forward, None);
        };
        let Some(l) = &self.live_sel else { return };
        if l.path != path {
            return; // stream not on the selected clip (e.g. mid-swap)
        }
        let d = d.or(l.duration).filter(|d| *d > 0.05);
        let pos = match d {
            Some(d) => l.position().rem_euclid(d),
            None => l.position(),
        };
        let n = times.len();
        // Base the step on the navigation intent, not the raw decoder
        // position: a prior keyframe step landed just *before* its target
        // start, so position alone reports the previous chapter and every
        // forward press would restep to the same place (stuck).
        let nav = self
            .chapters
            .as_ref()
            .filter(|b| b.path == path)
            .and_then(|b| b.nav);
        let pos_idx = times.partition_point(|&t| t <= pos).saturating_sub(1);
        let cur = resolve_chapter(pos_idx, nav);
        let target = if forward {
            (cur + 1) % n
        } else if pos - times[cur] > CHAPTER_RESTART_S {
            cur
        } else {
            (cur + n - 1) % n
        };
        // Stay a frame short of the end, like chapter_seek: a seek to the
        // tail lands on EOF and loops back to 0:00.
        let t = match d {
            Some(d) => times[target].clamp(0.0, (d - 0.05).max(0.0)),
            None => times[target].max(0.0),
        };
        l.player.seek(t, false);
        self.skip_flash_at = Some(Instant::now());
        // Reveal the bar as a timed peek so the step shows where it
        // landed (a bar already up via `g` stays sticky), then glide its
        // strip to center the landed-on chapter.
        self.open_chapter_bar(true);
        let (lo, hi) = self.chapter_pos_bounds(n);
        if let Some(b) = self.chapters.as_mut().filter(|b| b.path == path) {
            // Pin the intent so the highlight lands on `target` despite the
            // keyframe undershoot, and the next step advances from it.
            b.nav = Some(target);
            if b.open && b.times.is_some() {
                b.target = (target as f32).clamp(lo, hi);
            }
        }
        self.wake(1.5); // outlive the flash bar's hold + fade
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

    /// Keep the render loop awake for at least `secs` (covers fades and
    /// settle timers that plain spring-residual checks can't see).
    fn wake(&mut self, secs: f32) {
        self.wake_until = self
            .wake_until
            .max(Instant::now() + Duration::from_secs_f32(secs));
    }

    /// Keyboard/programmatic zoom. It has no grip to hold the grid by, so
    /// it also returns the layout to the plain justified packing — the way
    /// back to a clean grid after any amount of ribbon browsing.
    fn set_zoom(&mut self, target: f32) {
        let t = &self.tuning;
        self.zoom_target = target.clamp(t.zoom_min, t.zoom_max);
        self.ribbon_reset();
    }

    // --- per-frame ---

    fn drain_ingest(&mut self) {
        let Some(rx) = &self.rx else { return };
        // The D swap's clip streamed back in this drain (only field
        // updates are legal while `rx` borrows self; the selection move
        // happens after the batch installs and indices are final).
        let mut reselect: Option<PathBuf> = None;
        let mut incoming: Vec<Clip> = Vec::new();
        loop {
            // Per-frame budget (P0.3): leave the rest for the next frame
            // — which the wake guarantees — instead of stalling this one.
            if incoming.len() >= INGEST_DRAIN_BUDGET {
                self.waker.wake();
                break;
            }
            match rx.try_recv() {
                Ok(item) => {
                    if self.pending_reselect.as_deref() == Some(item.path.as_path()) {
                        self.pending_reselect = None;
                        reselect = Some(item.path.clone());
                    }
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
                    incoming.push(Clip {
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
                        aspect: None,
                        created: item.created,
                    });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    log::info!(
                        "stdin closed — {} clips ingested",
                        self.clips.len() + incoming.len()
                    );
                    self.rx = None;
                    // The awaited clip never showed up (deleted/moved):
                    // stop shielding its stream; update_live reaps it.
                    self.pending_reselect = None;
                    break;
                }
            }
        }
        if !incoming.is_empty() {
            let first_new = self.install_arrivals(incoming);
            // Spawn fade-in only needs the loop hot if a newcomer is
            // actually on screen — a bulk stream landing offscreen must
            // not keep the GPU presenting (P0.2). Offscreen arrivals
            // still repaint once (their send fired the waker) so the
            // title count stays fresh.
            let lay = self.layout();
            let (_, last_vis) = self.visible_rows(&lay, PREFETCH_ROWS);
            if self.row_of(&lay, first_new) <= last_vis {
                self.wake(0.6);
            }
        }
        if let Some(p) = reselect
            && let Some(&i) = self.index.get(&p)
        {
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

    /// Install one drained batch of arrivals. With sorting off (or
    /// disarmed by a shuffle) this is the classic append — sacred
    /// stdin/CLI order, no remap. With `--sort` armed, the batch merges
    /// into the creation-date-sorted library: one stable merge plus one
    /// index remap per FRAME (never per item — a per-item shift would go
    /// quadratic over a big stream), and surviving tiles keep their
    /// reflow entries so late-arriving older/newer files push them aside
    /// with a glide, not a snap. Returns the smallest new index (the
    /// visibility probe for the spawn-fade wake).
    fn install_arrivals(&mut self, mut incoming: Vec<Clip>) -> usize {
        let append_at = self.clips.len();
        if self.sort == SortMode::None || !self.sort_armed {
            for (k, c) in incoming.into_iter().enumerate() {
                self.index.insert(c.path.clone(), append_at + k);
                self.clips.push(c);
            }
            return append_at;
        }
        // Merge key: creation date (newest = descending), unknown last,
        // arrival order as the stable tie-break.
        let mode = self.sort;
        let key = move |c: &Clip| -> (bool, i128) {
            let nanos = c.created.map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as i128)
                    .unwrap_or_else(|e| -(e.duration().as_nanos() as i128))
            });
            match (mode, nanos) {
                (_, None) => (true, 0),
                (SortMode::Newest, Some(n)) => (false, -n),
                (_, Some(n)) => (false, n),
            }
        };
        incoming.sort_by_key(|c| key(c));
        let old: Vec<Clip> = std::mem::take(&mut self.clips);
        let n = old.len();
        let mut new_of_old: Vec<Option<usize>> = vec![None; n];
        let mut new_positions: Vec<usize> = Vec::with_capacity(incoming.len());
        let mut merged: Vec<Clip> = Vec::with_capacity(n + incoming.len());
        let mut inc = incoming.into_iter().peekable();
        for (o, c) in old.into_iter().enumerate() {
            // Existing clips win ties: they arrived first.
            while inc.peek().is_some_and(|i| key(i) < key(&c)) {
                new_positions.push(merged.len());
                merged.push(inc.next().unwrap());
            }
            new_of_old[o] = Some(merged.len());
            merged.push(c);
        }
        for c in inc {
            new_positions.push(merged.len());
            merged.push(c);
        }
        self.clips = merged;
        self.remap_refs(&new_of_old);
        for &p in &new_positions {
            self.index.insert(self.clips[p].path.clone(), p);
        }
        new_positions.first().copied().unwrap_or(append_at)
    }

    fn step(&mut self, dt: f32) {
        let t = self.tuning.clone();
        // Animation level "none": every chase/spring covers its full
        // distance in one frame — the UI snaps instead of tweening.
        let ui = self.level().ui();
        let a_of = |k: f32| if ui { alpha(k, dt) } else { 1.0 };

        // Zoom, anchored so the content at the viewport center stays put
        // while tile size (and row packing) reflows around it. In wrap mode
        // (flexible grid) the LAYOUT zoom snaps straight to its target and
        // the per-clip reflow tweens (rect glide + row-wrap slide) carry all
        // the visible motion — one repack, one wrap per crossing tile, like
        // the prototype tweening two settled layouts. Springing the zoom
        // instead repacked the rows at every intermediate value, restarting
        // a far-down tile's wrap several times per glide from an already-
        // snapped rect: a flickery ghost-fade, then a snap at the spring
        // tail. The fixed grid keeps the spring + crossfade.
        let old_zoom = self.zoom;
        let wrap_mode = ui && t.zoom_wrap && t.grid_layout == GridStyle::Flexible;
        // A live ribbon owns the zoom outright (the pinch sets it) and its
        // grip — not the camera — is what holds the view still.
        let ribboning = self.ribbon.is_some() || self.ribbon_settle.is_some();
        if ribboning {
            // leave zoom where the gesture put it
        } else if wrap_mode {
            self.zoom = self.zoom_target;
        } else {
            self.zoom += (self.zoom_target - self.zoom) * a_of(t.zoom_smoothing);
        }
        let zoomed = (self.zoom - old_zoom).abs() > 1e-5;
        if zoomed && !ribboning {
            let old_h = self.content_height(&self.layout_with(old_zoom));
            let new_h = self.content_height(&self.layout_with(self.zoom));
            if old_h > 0.0 {
                let half = self.viewport.height * 0.5;
                self.scroll = (self.scroll + half) / old_h * new_h - half;
                self.scroll_target = (self.scroll_target + half) / old_h * new_h - half;
            }
        }

        // Ribbon pinch: end a gesture that has gone quiet and advance its
        // release morph BEFORE the layout is captured — installing the
        // packing changes the layout, and a frame that captured the old
        // one would seed the wrap table with stale rects and then glide
        // the whole grid to the new ones (a visible slide after the
        // gesture had already come to rest).
        self.step_ribbon(dt);

        let lay = self.layout();

        // Optional extra inertia (off by default; macOS supplies momentum).
        if t.pan_inertia > 0.0 && self.last_scroll_event.elapsed().as_secs_f32() > 0.04 {
            self.scroll_target += self.scroll_vel * dt;
            self.scroll_vel *= t.pan_inertia.powf(dt * 60.0);
        }

        // Rubber-band the target back into bounds. A ribbon gesture pins
        // the view by its grip instead: the content height changes under
        // every pinch frame, and letting the clamp chase it would slide
        // the whole grid out from under the fingers.
        let max = self.max_scroll(&lay);
        if ribboning {
            self.scroll_target = self.scroll;
            self.scroll_vel = 0.0;
        } else if self.scroll_target < 0.0 {
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
        // No hover while pinching: the fingers are driving a gesture, not
        // resting on a tile, and the tile under them changes as the grid
        // moves — which cold-spawned a hover decoder per frame.
        let hover_now = if modal || self.chapters.is_some() || self.ribbon.is_some() {
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

        // Per-clip zoom-reflow: smooth re-justify glides + row-wrap events.
        // Suppressed while a ribbon is in flight — the ribbon rows ARE the
        // animation, and a glide chasing the justified layout underneath
        // would fight them.
        if self.ribbon.is_none() && self.ribbon_settle.is_none() {
            self.step_reflow(dt, &lay, zoomed);
        }

        // Quickview filmstrip slides with the same curve as keyboard moves.
        // While a scroll/scrub gesture is live the strip eases toward the
        // free-float `strip_target`; once the gesture settles the target homes
        // on the selected chip, so wheel input flows and then snaps magnetic.
        // In peek mode (strip_scroll_selects = false) a peeked strip HOLDS
        // where the user left it — it only homes once the selection moves
        // again (chip click, keyboard), which is what re-centers the strip.
        if self.quickview {
            let home = t.strip_scroll_selects || self.sel_changed_at >= self.last_scroll_event;
            if home && self.last_scroll_event.elapsed().as_secs_f32() > 0.12 {
                self.strip_target = self.selected as f32;
            }
            self.strip_pos += (self.strip_target - self.strip_pos) * a_of(t.strip_snap_strength);
            if (self.strip_target - self.strip_pos).abs() > 0.001 {
                self.motion = true;
            }
        }

        // A peeked bar (revealed by a chapter step) slides itself back
        // down once its window elapses; a sticky `g`-opened bar has no
        // deadline.
        if let Some(b) = &mut self.chapters
            && b.open
            && b.peek_until.is_some_and(|t| Instant::now() >= t)
        {
            b.open = false;
            b.peek_until = None;
            self.wake(0.8);
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

    /// Displayed aspect (w/h) of clip `i`: the thumbnail's true aspect
    /// (fit-scaled, rotation already applied), clamped so a pathological
    /// source stays browsable; 16:9 until the shape is known. Sizes both
    /// strip chips and the flexible grid's tiles — fixed height, width
    /// varies, portrait clips are narrower, never taller. Sources, in
    /// order: the resident thumb, the session-persistent `Clip.aspect`
    /// (so atlas eviction never reshapes anything), then the memoized
    /// probe meta — the chapter bar images its chips from the anim sheet,
    /// which this shape lookup doesn't consult, so a clip reached without
    /// its static thumb atlas-resident (jumped to via random/shuffle, or
    /// the bar opened before the thumb landed) relies on the meta's real
    /// dims to avoid a 16:9 default.
    fn chip_aspect(&self, i: usize) -> f32 {
        let c = self.clips.get(i);
        let a = match c.map(|c| &c.thumb) {
            Some(&Thumb::Ready { tw, th, .. }) => tw as f32 / (th as f32).max(1.0),
            _ => c
                .and_then(|c| c.aspect)
                .or_else(|| c.and_then(|c| self.meta_aspect(&c.path)))
                .unwrap_or(16.0 / 9.0),
        };
        a.clamp(9.0 / 16.0, 2.4)
    }

    /// Displayed aspect (w/h) from the memoized probe meta, rotation
    /// applied (±90/±270 swap the coded dims): the shape a generated
    /// thumb's `meta.json` records. Memory-only (`meta_cache`), so safe on
    /// the render thread; `None` until a spawn has read the clip's meta.
    fn meta_aspect(&self, path: &std::path::Path) -> Option<f32> {
        let m = self.meta_cache.get(path)?;
        Self::meta_aspect_of(m)
    }

    /// Rotation-aware displayed aspect of a probe meta, `None` unless both
    /// coded dims are present.
    fn meta_aspect_of(m: &sb_media::Meta) -> Option<f32> {
        let (w, h) = (m.width? as f32, m.height? as f32);
        // ±90/±270 (rotation may be signed) swap the coded dims.
        let quarter = m.rotation.is_some_and(|r| (r / 90.0).round() as i64 % 2 != 0);
        let (w, h) = if quarter { (h, w) } else { (w, h) };
        (h > 0.0).then(|| w / h)
    }

    /// Pin `Clip.aspect` for the selected clip from its probe meta when the
    /// thumb never set it (off-screen jump target). One memoized read —
    /// after it lands, `aspect.is_some()` short-circuits every later call.
    fn resolve_selected_aspect(&mut self) {
        let Some(c) = self.clips.get(self.selected) else {
            return;
        };
        if c.aspect.is_some() {
            return;
        }
        let path = c.path.clone();
        if let Some(a) = self.clip_meta(&path).as_ref().and_then(Self::meta_aspect_of) {
            self.clips[self.selected].aspect = Some(a);
            // A newly-known aspect reshapes the flexible grid; the key
            // can't see it, so bump the revision (like the thumb arm).
            self.grid_rev = self.grid_rev.wrapping_add(1);
        }
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
        let nrows = self.rows(lay);
        if nrows == 0 {
            return (0, 0);
        }
        let first = self.row_at_y(lay, self.scroll);
        let last = self.row_at_y(lay, self.scroll + self.viewport.height);
        (
            first.saturating_sub(margin),
            (last + margin + 1).min(nrows - 1),
        )
    }

    /// Zone rows ordered center-outward, for prioritized requests.
    fn zone_rows(&self, lay: &Layout) -> Vec<usize> {
        let (first_row, last_row) = self.visible_rows(lay, PREFETCH_ROWS);
        let center = self.row_at_y(lay, self.scroll + self.viewport.height * 0.5) as i64;
        let mut rows: Vec<usize> = (first_row..=last_row).collect();
        rows.sort_by_key(|r| (*r as i64 - center).abs());
        rows
    }

    /// Queue thumbnail generation for visible + nearby tiles, center-out,
    /// within the atlas slot budget. Without the budget, a big viewport
    /// demands more slots than exist and eviction churns everything
    /// forever.
    fn request_visible_thumbs(&mut self, lay: &Layout) {
        if self.demo {
            return;
        }
        let rows = self.zone_rows(lay);
        let mut budget = self.atlas_cfg.slots() as i64 - 8; // headroom incl. live slot

        'statics: for &row in &rows {
            for i in self.row_range(lay, row) {
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

    }

    /// The ONLY place anim sheets are requested: quickview/fullview ask
    /// for the selected clip's sheet on demand (g² cheap seeked extracts,
    /// disk-cached) — the seekbar's storyboard preview and the chapter
    /// bar's chips sample it. One clip at a time, never library-wide
    /// (the old bulk sweep is gone: clips the user never opens never pay
    /// for a sheet, and the freed workers finish the thumb sweep sooner).
    /// `request_anim_now` runs above the gen sweep, which can back up
    /// for hours after a recipe change — the user hovering the seekbar
    /// can't wait behind that.
    fn request_quickview_sheet(&mut self) {
        // Fullview wants the sheet too — the chapter bar's chips sample
        // it, and requesting on fullview ENTRY (not bar-open) means the
        // storyboard is generating/cached before g is ever pressed. But
        // never before the video being watched has its first frame
        // (prewarm_ok): sheet generation is nine niced ffmpeg decodes,
        // and racing them against the interactive cold spawn is exactly
        // the jank the priority tiers exist to prevent.
        // --no-storyboards short-circuits the ONLY sheet-request site: no
        // anim atlas is ever queued, so the gen sweep and the interactive
        // cold spawns keep the freed workers.
        if self.no_storyboards {
            return;
        }
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
        let presenting = self
            .live_sel
            .as_ref()
            .is_some_and(|l| l.first_frame.is_some())
            && !self.sel_parked
            && !self.paused();
        // The gen-sweep throttle engages the moment we're TRYING to watch —
        // including the watched stream's cold spawn, BEFORE its first frame.
        // Gating it on `first_frame` (like the upload budget below) created a
        // feedback loop: the watched 4K stream stalls under the full-width
        // sweep, never reaches its first frame, so the cap never engages, so
        // the sweep keeps running 3-wide and stalls it (and the storyboard)
        // harder — chapter chips took ~a minute to appear
        // (benchmarks/reports/chapter_sheet_latency.md). Keying the throttle
        // on "a selected stream exists and we're not parked/paused" holds the
        // cap across the whole spawn, so the stream reaches first frame fast.
        let watching = self.live_sel.is_some() && !self.sel_parked && !self.paused();
        self.media.set_live(watching);
        // The upload-budget squeeze, unlike the throttle, only matters once
        // frames are actually arriving to upload — keep it on `presenting`.
        if presenting {
            self.probe
                .counters
                .drain_budget_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let budget = self.media_upload_budget(presenting);
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
                    if !self.clips[i].cached {
                        self.clips[i].cached = true;
                        self.probe
                            .counters
                            .thumbs_cached
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
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
                    // The clip's true aspect just became known — remember
                    // it past eviction and reflow the flexible grid.
                    let a = w as f32 / (h as f32).max(1.0);
                    if self.clips[i].aspect != Some(a) {
                        self.clips[i].aspect = Some(a);
                        self.grid_rev = self.grid_rev.wrapping_add(1);
                    }
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
                    if let Some(&i) = self.index.get(&path)
                        && !self.clips[i].cached
                    {
                        self.clips[i].cached = true;
                        self.probe
                            .counters
                            .thumbs_cached
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                ThumbResult::Unplayable { path } => {
                    // Gatekeeper stage two: it wore a valid extension and a
                    // plausible header, but ffmpeg couldn't pull a single
                    // frame from it (or the file vanished) — off the grid.
                    // Cloud placeholders are exempt: their data is
                    // legitimately absent, not corrupt.
                    if self.tuning.gatekeeper
                        && let Some(&i) = self.index.get(&path)
                        && !self.clips[i].cloud
                    {
                        log::info!("gatekeeper: dropping unplayable {}", path.display());
                        self.remove_clip(i);
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
        let center_row = self.row_at_y(lay, self.scroll + self.viewport.height * 0.5) as i64;
        let (zone_first, zone_last) = self.visible_rows(lay, PREFETCH_ROWS);
        let mut best: Option<(usize, u8, i64)> = None;
        for (j, owner) in self.slots.iter().enumerate() {
            let Some((owner, kind)) = owner else {
                return Some(j);
            };
            let row = self.row_of(lay, *owner);
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
            self.probe
                .counters
                .evictions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        let paused = self.paused();
        let live_on = !self.demo && !paused && self.level().live();
        // Focus-pause PARKS the selected stream and the warm pool
        // instead of killing them: an undrained lane's bounded queue
        // stalls its decoder within a few frames (same near-zero cost as
        // offscreen parking), and refocus resumes the very same decoders
        // on their own timelines — late frames re-anchor, so playback
        // continues from where it stopped. Reaping here (the old
        // behavior) made every refocus pay a cold respawn that visibly
        // jumped the video back to the thumbnail's frame.
        let paused_live = paused && !self.demo && self.level().live();
        // The grid is mid-gesture (ribbon pinch or its settle): every frame
        // repacks it, so any neighbour derived from the layout is noise.
        let ribboning = self.ribbon.is_some() || self.ribbon_settle.is_some();
        let delay_ms = self.tuning.live_delay_ms;
        // Attention mode (DESIGN.md §15 spike): the hires lane follows
        // attention — the hovered tile while mousing, the selection while
        // keyboard-navigating. `attn_hover` marks a hover-driven target,
        // which settles on the hover clock with its own (longer) guard:
        // hover is volatile and every settle is a quickview-res spawn.
        let attn_hover = self.attention()
            && !self.quickview
            && !self.fullview
            && self.mouse_attention
            && live_on;
        let sel_target = if live_on {
            self.attention_target()
        } else if paused_live {
            // Parked, not dead: keep pointing at the lane's own clip so
            // the stop-lanes sweep below skips it. A lane-less pause
            // yields None, so nothing spawns while unfocused.
            self.live_sel.as_ref().map(|l| l.clip)
        } else {
            None
        };
        // The hover lane: in the grid, the hovered tile; in quickview,
        // the hovered filmstrip chip. Never the selected clip (its
        // stream owns that). Attention mode deletes the grid's tile-size
        // hover lane — the attention lane IS grid hover playback (strict
        // rule: nothing else ever plays in the grid).
        let hover_target = if !live_on {
            None
        } else if self.quickview {
            self.strip_hover.filter(|h| *h != self.selected)
        } else if self.attention() {
            None
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

        // Pre-warm decoders for the likely next move so a selection change
        // shows video instantly instead of paying a cold spawn (open + decode
        // to the thumb frame — no longer the CLI's ~1s floor, but still
        // visible latency). Kept deliberately small: every warm lane is a 4K
        // cold spawn that competes with the watched stream and any in-flight
        // storyboard for CPU/VT, so we warm only the moves that are actually
        // common — 'next' everywhere, 'down' in the grid.
        let mut warm_targets: Vec<usize> = Vec::new();
        // Computed while paused too (paused_live): the warm pool rides
        // out a focus loss parked — its players are undrained and
        // stalled by design, so keeping them costs nothing and refocus
        // finds every movement destination still warm. Fresh spawns
        // stay gated on `live_on` below.
        if (live_on || paused_live) && !pending && !ribboning {
            let s = self.selected;
            let n = self.clips.len();
            // The "down" destination: the horizontally nearest tile in
            // the next row — same-column in the fixed grid, the visually
            // adjacent tile under flexible rows (mirrors move_selection).
            let down = {
                let row = self.row_of(lay, s);
                let (x, _, w, _) = self.tile_rect(lay, s);
                (row + 1 < self.rows(lay))
                    .then(|| self.nearest_in_row(lay, row + 1, x + w * 0.5))
                    .flatten()
            };
            let mut push = |i: usize| {
                if i < n && i != s && !warm_targets.contains(&i) {
                    warm_targets.push(i);
                }
            };
            // Warm-ups run one at a time, so this order is a priority.
            // 'next' (right) is the one move common to every view — grid
            // advance and filmstrip step both flow right — so warm it
            // everywhere. 'down' is a grid-only clip jump (the modals move by
            // strip / chapter / view-depth, never a vertical clip hop), so
            // warm it only in the grid. 'up', second-right, and 'left' stay
            // cold: rare, and not worth a 4K decoder against the watched
            // stream / storyboard.
            push(s + 1);
            if !self.quickview
                && !self.fullview
                && let Some(d) = down
            {
                push(d);
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
        // Track the hover preview's position for the continuity handoff:
        // refreshed every frame while the preview plays, and after the
        // lane dies (clicking the hovered tile makes hovered == selected,
        // which reaps it right below) the last position SURVIVES as long
        // as the selection is on that clip — the selected-lane open may
        // trail the click by the settle delay, so a frame-local capture
        // would miss the fresh-spawn path.
        self.hover_resume = self
            .live_hover
            .as_ref()
            .filter(|h| h.first_frame.is_some())
            .map(|h| (h.clip, h.player.position()))
            .or(self
                .hover_resume
                .filter(|&(hc, _)| sel_target == Some(hc)));
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
                // Continuity handoff: if this clip's hover preview was
                // playing (it usually died THIS frame — clicking the
                // hovered tile makes it the selection, which reaps the
                // lane above), the user has been watching it advance past
                // the thumb anchor — but the warm stream spawned at the
                // anchor and has been parked ever since, so promoting its
                // queued frames visibly jumped playback backward by
                // however long the preview ran (measured: ~6s on a 4s-GOP
                // 4K clip; benchmarks/scenarios/hover_then_select_handoff
                // .toml). Keyframe-seek the promoted stream to the
                // preview's position in place (~10-30ms, no respawn):
                // playback continues from what's on screen, at worst one
                // GOP behind it.
                if let Some((hc, hp)) = self.hover_resume
                    && hc == i
                    && (hp - l.player.position()).abs() > 0.5
                {
                    l.player.seek(hp, false);
                }
                let clip: Arc<str> = Arc::from(l.path.to_string_lossy().as_ref());
                self.probe.mark_pts(
                    sb_media::EventKind::Promotion,
                    sb_media::Lane::Selected,
                    l.generation,
                    &clip,
                    l.player.position(),
                );
                self.live_sel = Some(l);
            } else if self.quickview
                || if attn_hover {
                    // Hover-driven attention settles on the hover clock,
                    // behind its own longer guard (cold-spawn churn).
                    self.hover_changed_at.elapsed().as_millis() as f32
                        >= self.tuning.attention_delay_ms
                } else {
                    self.sel_changed_at.elapsed().as_millis() as f32 >= delay_ms
                }
            {
                let resume = self
                    .hover_resume
                    .filter(|&(hc, _)| hc == i)
                    .map(|(_, p)| p);
                self.live_sel = self.start_sel_live(i, sb_media::Lane::Selected, resume);
            }
        }
        // A ribbon pinch moves the zoom EVERY frame, so the layout — and
        // with it the "down" neighbour — is different on every one. Left
        // alone, the pool dropped and respawned a decoder per frame for
        // the whole gesture (a VT session and its thread each time), which
        // is what ground the machine down mid-pinch. The targets are also
        // computed from the justified layout, which isn't even what the
        // ribbon is drawing. So hold the pool exactly as it is until the
        // grid settles, like the D-swap shield above.
        if !ribboning {
            self.warm.retain(|w| warm_targets.contains(&w.clip));
        }
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
        if live_on // parked pause keeps the pool but never spawns into it
            && sel_ready
            && !warming_up
            && self.sel_changed_at.elapsed().as_millis() as f32 >= delay_ms
            && let Some(&i) = warm_targets
                .iter()
                .find(|&&i| self.warm.iter().all(|w| w.clip != i))
            && let Some(l) = self.start_sel_live(i, sb_media::Lane::Warm, None)
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

        let mut hover_served: Option<(u64, usize, f64)> = None;
        if let Some(live) = &mut self.live_hover
            && let Some(rgba) = live.player.take_frame()
        {
            if live.first_frame.is_none() {
                live.first_frame = Some(Instant::now());
                // Handoff diagnostic (see the sel-lane twin below).
                log::debug!(
                    "hover live clip {} first frame, pos={:.3}s",
                    live.clip,
                    live.player.position(),
                );
                hover_served = Some((live.generation, live.clip, live.player.position()));
            }
            if live.served < 6
                && let Some(c) = self.clips.get(live.clip)
            {
                handoff_dump(
                    &c.path.file_stem().unwrap_or_default().to_string_lossy(),
                    "hover",
                    live.generation,
                    live.served,
                    live.player.position(),
                    live.player.w,
                    live.player.h,
                    &rgba,
                    self.media.cached_thumb_path(&c.path),
                );
            }
            live.served += 1;
            uploads.push(ThumbUpload {
                slot: live.slot,
                w: live.player.w,
                h: live.player.h,
                rgba,
            });
        }
        if let Some((generation, ci, pos)) = hover_served
            && let Some(path) = self.clips.get(ci).map(|c| c.path.clone())
        {
            let clip: Arc<str> = Arc::from(path.to_string_lossy().as_ref());
            self.probe.mark_pts(
                sb_media::EventKind::FrameServed,
                sb_media::Lane::Hover,
                generation,
                &clip,
                pos,
            );
        }
        // Park the selected stream while its tile is offscreen and no
        // modal shows it (P0.4), or whenever focus-pause is active (the
        // defocus fix): stop draining and the bounded queue stalls the
        // decoder — decode, copy, upload and mip generation all cease —
        // while the stream object and its timeline survive. Refocus or
        // panning back resumes the same decoder; no respawn. (Keyboard
        // moves always scroll to the selection, so only trackpad panning
        // reaches the offscreen half.)
        self.sel_parked = paused && self.live_sel.is_some()
            || self.live_sel.is_some() && !self.quickview && !self.fullview && {
                let (first, last) = self.visible_rows(lay, 0);
                // Park by the LANE's tile, not the selection: in attention
                // mode the lane may be playing the hovered tile (which is
                // visible by definition, so it never parks). Lane indices can
                // go stale mid-D-swap — fall back to the selection then.
                let i = self
                    .live_sel
                    .as_ref()
                    .map(|l| l.clip)
                    .filter(|&c| c < self.clips.len())
                    .unwrap_or(self.selected);
                let row = self.row_of(lay, i);
                !(first..=last).contains(&row)
            };
        // Reclaim the previously presented hires buffer (P1.5): the
        // renderer dropped its Frame after the upload, so once we hold
        // the only Arc, the buffer goes back to the playing decoder's
        // pool. If a modal swap left no player (or the Arc is still in
        // flight after a skipped present), it just drops — old behavior.
        if self
            .hires_reclaim
            .as_ref()
            .is_some_and(|a| Arc::strong_count(a) == 1)
            && let Some(a) = self.hires_reclaim.take()
            && let Ok(buf) = Arc::try_unwrap(a)
            && let Some(l) = &self.live_sel
        {
            l.player.recycle(buf);
        }
        let mut sel_served: Option<(u64, PathBuf, f64)> = None;
        if !self.sel_parked
            && let Some(live) = &mut self.live_sel
            && let Some(rgba) = live.player.take_frame()
        {
            if live.first_frame.is_none() {
                live.first_frame = Some(Instant::now());
                // pos = the served frame's content-relative pts: the
                // handoff diagnostic (compare against the spawn's "@Ts"
                // target — a mismatch beyond one GOP means the thumb and
                // the live open disagree about where the clip starts).
                log::debug!(
                    "sel live clip {} first frame {:.0}ms after spawn, pos={:.3}s",
                    live.clip,
                    live.spawned.elapsed().as_secs_f32() * 1000.0,
                    live.player.position(),
                );
                sel_served = Some((live.generation, live.path.clone(), live.player.position()));
            }
            if live.served < 6 {
                handoff_dump(
                    &live.path.file_stem().unwrap_or_default().to_string_lossy(),
                    "sel",
                    live.generation,
                    live.served,
                    live.player.position(),
                    live.player.w,
                    live.player.h,
                    &rgba,
                    self.media.cached_thumb_path(&live.path),
                );
            }
            live.served += 1;
            if self.hires_shown.as_ref() != Some(&live.path) {
                self.hires_shown = Some(live.path.clone());
            }
            let rgba = Arc::new(rgba);
            self.hires_reclaim = Some(rgba.clone());
            self.hires_frame = Some(HiresFrame {
                w: live.player.w,
                h: live.player.h,
                rgba,
            });
        }
        if let Some((generation, path, pos)) = sel_served {
            let clip: Arc<str> = Arc::from(path.to_string_lossy().as_ref());
            self.probe.mark_pts(
                sb_media::EventKind::FrameServed,
                sb_media::Lane::Selected,
                generation,
                &clip,
                pos,
            );
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
    /// hardware. A stale `src` (rename under size_mtime keying) queues
    /// the same reprobe — the worker rewrites it, since `cached_meta` no
    /// longer heals inline (P0.7: that was a write on the render thread).
    /// Never probe on the render thread — that's a hitch.
    fn heal_meta(&mut self, meta: Option<&sb_media::Meta>, path: &std::path::Path) {
        if meta.is_some_and(|m| m.pix_fmt.is_none() || m.src != path)
            && self.reprobed.insert(path.to_path_buf())
        {
            self.media.request_reprobe(path.to_path_buf());
        }
    }

    /// `cached_meta` behind the session memo (P0.7): the first read of a
    /// clip pays the disk (source stat + meta.json — acceptable once, on
    /// an explicit attention move); every spawn after that is memory.
    /// Only complete metas (pix_fmt present) are memoized, so pre-heal
    /// entries keep re-reading until the background reprobe lands.
    fn clip_meta(&mut self, path: &std::path::Path) -> Option<sb_media::Meta> {
        if let Some(m) = self.meta_cache.get(path) {
            return Some(m.clone());
        }
        let m = sb_media::cached_meta(path)?;
        if m.pix_fmt.is_some() {
            self.meta_cache.insert(path.to_path_buf(), m.clone());
        }
        Some(m)
    }

    /// The selected clip's decoder: natural resolution, capped at the hires
    /// texture (never upscaled past the source when its dims are known).
    /// Resident and seekable — repositioning after this is `player.seek()`,
    /// never another spawn.
    /// Mint a lane-incarnation id, emit its `DecodeSpawn` event, and
    /// attach the probe context so the player's reader thread can tag its
    /// own media events (first-frame-ready, re-anchors) with this lane's
    /// identity. Returns the generation to stamp on the lane state.
    fn instrument_lane(
        &mut self,
        player: &sb_media::SeekablePlayer,
        lane: sb_media::Lane,
        path: &std::path::Path,
    ) -> u64 {
        let generation = self.next_lane_gen;
        self.next_lane_gen = self.next_lane_gen.wrapping_add(1);
        let clip: Arc<str> = Arc::from(path.to_string_lossy().as_ref());
        self.probe
            .mark(sb_media::EventKind::DecodeSpawn, lane, generation, &clip);
        player.attach_probe(sb_media::LaneProbe {
            sink: self.probe.clone(),
            lane,
            generation,
            clip,
        });
        generation
    }

    fn start_sel_live(
        &mut self,
        i: usize,
        lane: sb_media::Lane,
        resume: Option<f64>,
    ) -> Option<SelLive> {
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
        let meta = self.clip_meta(&path);
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
        // frame the tile already shows instead of jolting to 0:00 — UNLESS
        // the caller passed a resume position: then this clip's hover
        // preview was just on screen at that position, and opening at the
        // anchor visibly jumped playback backward by however long the
        // preview ran (the continuity handoff — see the promotion twin).
        let duration = meta.as_ref().and_then(|m| m.duration);
        let seek = resume.unwrap_or_else(|| {
            duration
                .map(|d| (d * self.seek_fraction).max(0.0))
                .unwrap_or(0.0)
        });
        self.heal_meta(meta.as_ref(), &path);
        let player = sb_media::SeekablePlayer::spawn(&path, dw, dh, seek, meta.as_ref())?;
        // Deadline-paced redraws (P1.4): a frame landing in a dry queue
        // must nudge the sleeping loop.
        player.set_notify(self.notify.clone());
        let generation = self.instrument_lane(&player, lane, &path);
        log::debug!(
            "selected live {dw}x{dh} @{seek:.3}s (dur {:?} × frac {:.3}): {}",
            duration,
            self.seek_fraction,
            path.display()
        );
        Some(SelLive {
            clip: i,
            path,
            player,
            spawned: Instant::now(),
            first_frame: None,
            duration,
            generation,
            served: 0,
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
        let meta = self.clip_meta(&path);
        let seek = meta
            .as_ref()
            .and_then(|m| m.duration)
            .map(|d| (d * self.seek_fraction).max(0.0))
            .unwrap_or(0.0);
        self.heal_meta(meta.as_ref(), &path);
        let Some(player) = sb_media::SeekablePlayer::spawn(&path, tw, th, seek, meta.as_ref())
        else {
            log::debug!("live preview failed to start: {}", path.display());
            return None;
        };
        player.set_notify(self.notify.clone()); // P1.4 dry-queue wake
        let generation = self.instrument_lane(&player, sb_media::Lane::Hover, &path);
        log::debug!("hover live @{seek:.3}s: {}", path.display());
        self.slots[slot] = Some((i, SlotKind::Live));
        Some(LiveState {
            clip: i,
            player,
            slot,
            first_frame: None,
            generation,
            served: 0,
        })
    }

    fn update_title(&mut self) {
        let name = self
            .clips
            .get(self.selected)
            .and_then(|c| c.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Window-title label: the debug-quality label allowed pre-M7 (DESIGN.md §9).
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
        let reflow_on = self.reflow_active(&lay);
        let skip_bar = self.skip_bar();
        let mut tiles = Vec::new();
        // Z-order: grid tiles, then the hovered tile lifted above its
        // neighbors, then the selected tile on top of everything.
        let mut hovered_group: Vec<Tile> = Vec::new();
        let mut selected_group: Vec<Tile> = Vec::new();

        // Modal views freeze the world behind them: the backdrop is a
        // STATIC snapshot of the grid from the moment the modal opened
        // (quickview blurs it; fullview covers it entirely, so it draws
        // NOTHING), and none of the per-tile work below — dots, skip
        // bars — runs while one is up. A resize invalidates the snapshot
        // (it stores window positions).
        let modal = self.quickview || self.fullview;
        if !modal
            || self
                .frozen_grid
                .as_ref()
                .is_some_and(|(w, h, _)| *w != self.viewport.width || *h != self.viewport.height)
        {
            self.frozen_grid = None;
        }
        // Whether the active view wants the grid drawn at all: flat
        // backdrops (fullview's default, quickview's opt-in) cover it
        // with an opaque stage, so building it is invisible churn.
        let backdrop_grid = if self.fullview {
            self.tuning.fullview_backdrop == BackdropStyle::Blur
        } else if self.quickview {
            self.tuning.quickview_backdrop == BackdropStyle::Blur
        } else {
            true
        };
        if let Some((_, _, frozen)) = &self.frozen_grid {
            tiles = frozen.clone();
        } else if backdrop_grid {
            // While a ribbon pinch (or its release morph) is up, the rows it
            // lays out ARE the frame — a clip straddling a row boundary
            // appears in both rows, and its second copy rides the same slot
            // the wrap's exit copy uses. Otherwise: the visible layout rows.
            let ribbon_rows = self.ribbon_rows();
            // Borrowed out of self so the draw loop can read them freely,
            // and handed back below — the point is that they keep their
            // allocation instead of being rebuilt every frame.
            let mut ribbon_main = std::mem::take(&mut self.ribbon_main);
            let mut ribbon_second = std::mem::take(&mut self.ribbon_second);
            let draw: Vec<usize> = match &ribbon_rows {
                Some(rows) => {
                    if ribbon_main.len() != self.clips.len() {
                        ribbon_main.clear();
                        ribbon_second.clear();
                        ribbon_main.resize(self.clips.len(), None);
                        ribbon_second.resize(self.clips.len(), None);
                    }
                    let mut order = Vec::new();
                    for row in rows {
                        for it in &row.items {
                            let rect = [it.x, row.y + self.scroll, it.w, row.h];
                            let Some(slot) = ribbon_main.get_mut(it.clip) else {
                                continue;
                            };
                            match slot {
                                None => {
                                    *slot = Some(rect);
                                    order.push(it.clip);
                                }
                                Some(_) => ribbon_second[it.clip] = Some(rect),
                            }
                        }
                    }
                    order
                }
                None => (first_row..=last_row)
                    .flat_map(|r| self.row_range(&lay, r))
                    .collect(),
            };
            {
                for &i in &draw {
                    // Displayed rect: the smoothed reflow rect (so a zoom
                    // re-justify glides) when active, else the raw layout.
                    // A wrapping clip draws its enter copy here, slid in from
                    // the opposite window edge, and an exit copy at the push
                    // site below sliding off the edge it left.
                    let rf = self
                        .reflow
                        .get(i)
                        .copied()
                        .filter(|_| reflow_on)
                        .filter(|r| r.init);
                    let mut wrap_exit: Option<[f32; 4]> = None;
                    let (tx, ty, tw_g, th_g) = if let Some(r) =
                        ribbon_main.get(i).copied().flatten()
                    {
                        // Ribbon frame: this row's slot, plus the second
                        // copy when the clip straddles a row boundary.
                        wrap_exit = ribbon_second.get(i).copied().flatten();
                        (r[0], r[1], r[2], r[3])
                    } else {
                        match rf {
                        Some(r) => {
                            let vw = self.viewport.width;
                            let gp = t.gap;
                            if let Some(w) = r.wrap {
                                let ease = |x: f32| {
                                    let x = x.clamp(0.0, 1.0);
                                    if x < 0.5 {
                                        4.0 * x * x * x
                                    } else {
                                        1.0 - (-2.0 * x + 2.0).powi(3) * 0.5
                                    }
                                };
                                let stagger = r.row as f32 * t.zoom_wrap_stagger_ms;
                                let p = ease((w.elapsed - stagger) / t.zoom_wrap_ms.max(1.0));
                                // Enter copy slides in from the opposite edge.
                                let enter0 = if w.forward { -(r.rect[2] + gp) } else { vw + gp };
                                let ex = enter0 + (r.rect[0] - enter0) * p;
                                // Exit copy slides the old rect off its edge.
                                let exit1 = if w.forward { vw + gp } else { -(w.from[2] + gp) };
                                let mut fr = w.from;
                                fr[0] += (exit1 - w.from[0]) * p;
                                wrap_exit = Some(fr);
                                (ex, r.rect[1], r.rect[2], r.rect[3])
                            } else {
                                (r.rect[0], r.rect[1], r.rect[2], r.rect[3])
                            }
                        }
                        None => self.tile_rect(&lay, i),
                        }
                    };
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
                    // The tile the hires lane belongs to: the selection in
                    // classic mode, wherever attention points otherwise.
                    let attn_tile = self.attention_target() == Some(i);
                    let marked = self.marked.contains(&i);

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
                    // Never inside a frozen-backdrop capture (modal): the
                    // hires texture keeps updating as the modal plays the
                    // same stream, so a captured hires reference is a
                    // live window in a supposedly frozen snapshot — the
                    // selected tile visibly animated behind the blur.
                    // The static thumb is the honest frozen stand-in.
                    if (selected || attn_tile) && !modal {
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
                    let (wg, hg) = (tw_g * s, th_g * s);
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
                    let cx = tx + tw_g * 0.5;
                    let cy = ty - self.scroll + th_g * 0.5;

                    // Texture source: live hires when the tile is playing,
                    // the static thumb otherwise. Grid tiles never cycle
                    // anim-sheet frames — sheets exist only as quickview/
                    // fullview storyboard data now.
                    let mut mix = tex_mix;
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
                    } else if marked {
                        // Multi-selected (attention mode): the selection
                        // border, border-only — the tile never plays.
                        (
                            [sb[0], sb[1], sb[2], 0.9 * ease],
                            t.selection_border_width,
                            t.corner_radius,
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
                        uv2: [0.0; 4],
                        frame_fade: 0.0,
                        tex_mix: mix,
                        hires: tile_hires,
                        pie: 0.0,
                    };
                    let out = if selected {
                        &mut selected_group
                    } else if hovered {
                        &mut hovered_group
                    } else {
                        &mut tiles
                    };
                    // The exit copy: same clip/texture, positioned on the row
                    // it left (its uv crop matches — same aspect, so cloning
                    // and overriding the rect is exact). Drawn under the enter
                    // copy; the window bounds clip whatever slides off-edge.
                    if let Some(fr) = wrap_exit {
                        let mut exit = tile;
                        exit.x = fr[0];
                        exit.y = fr[1] - self.scroll;
                        exit.w = fr[2];
                        exit.h = fr[3];
                        out.push(exit);
                    }
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
            // Reset only what this frame wrote, then hand the buffers
            // back: clearing the whole table would be O(library) again.
            for &i in &draw {
                if let Some(slot) = ribbon_main.get_mut(i) {
                    *slot = None;
                }
                if let Some(slot) = ribbon_second.get_mut(i) {
                    *slot = None;
                }
            }
            self.ribbon_main = ribbon_main;
            self.ribbon_second = ribbon_second;
            drop(ribbon_rows);
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
        // The flexible grid wraps its zoom reflow (per-clip, above), so the
        // cols-change crossfade is only for the fixed grid here; shuffle and
        // the D swap still set `transition` directly at their call sites.
        if modal {
            self.transition = None;
        } else if ui && !reflow_on && lay.cols != self.last_cols && !self.last_tiles.is_empty() {
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

        // Quickview (DESIGN.md §6 level 3, internal): blur + dim everything
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
            let flat = t.quickview_backdrop == BackdropStyle::Flat;
            if !flat && t.quickview_blur >= 0.5 {
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
                pie: 0.0,
            };
            // Flat backdrop: an opaque stage instead of the tinted dim —
            // the grid behind it was never built, so nothing shows
            // through even mid-fade (the fade starts from the window
            // clear color).
            let bc = t.backdrop_color;
            tiles.push(if flat {
                full(0.0, 0.0, vw, vh, [bc[0], bc[1], bc[2], fade])
            } else {
                full(
                    0.0,
                    0.0,
                    vw,
                    vh,
                    [0.0, 0.0, 0.0, t.quickview_dim.clamp(0.0, 1.0) * fade],
                )
            });

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
                    pie: 0.0,
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
                    pie: 0.0,
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
                pie: 0.0,
            };
            // Flat (default): an opaque backdrop_color stage covers the
            // grid (and any quickview) behind it. Blur: the frozen grid
            // is already in `tiles` — frost it and lay quickview's
            // tinted dim on top instead, so fullview can wear the same
            // backdrop as the quickview modal.
            if self.tuning.fullview_backdrop == BackdropStyle::Blur {
                if t.quickview_blur >= 0.5 {
                    blur = Some(Blur {
                        split: tiles.len(),
                        levels: t.quickview_blur.round() as u32,
                        fade: 1.0,
                    });
                }
                tiles.push(full(
                    0.0,
                    0.0,
                    vw,
                    vh,
                    [0.0, 0.0, 0.0, t.quickview_dim.clamp(0.0, 1.0)],
                ));
            } else {
                let bc = t.backdrop_color;
                tiles.push(full(0.0, 0.0, vw, vh, [bc[0], bc[1], bc[2], 1.0]));
            }
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
                        pie: 0.0,
                    });
                    // The chapter bar claims the bottom edge while up, so
                    // the seekbar rides up above it (staying visible, via
                    // seekbar_lift) as the bar slides in instead of fading
                    // out under it.
                    let bar_a = self
                        .seekbar_alpha()
                        .max(skip_bar.map(|(_, a)| a).unwrap_or(0.0));
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
            && let Some((anim_src, sheet_coming)) = self.clips.get(self.selected).map(|c| {
                (
                    match c.anim {
                        Thumb::Ready { slot, tw, th, .. } => Some((slot, tw as f32, th as f32)),
                        _ => None,
                    },
                    // The sheet can still arrive: generating now, or not
                    // yet requested but requestable (a Failed THUMB means
                    // the sheet request will never fire — dots would spin,
                    // and wake the loop, forever).
                    matches!(c.anim, Thumb::Pending)
                        || (matches!(c.anim, Thumb::None) && !matches!(c.thumb, Thumb::Failed)),
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
                        pie: 0.0,
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
                        // timestamp. Cells are now the source's true aspect
                        // (= the chip's aspect), so this center-crop is
                        // essentially identity — it stays only to absorb
                        // rounding between the cell box and the chip shape,
                        // and never stretches.
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
                            pie: 0.0,
                        };
                        if cur {
                            cur_chip = Some(tile);
                        } else if hov {
                            hov_chip = Some(tile);
                        } else {
                            tiles.push(tile);
                        }
                        // Sheet still generating: dots on the chip (after
                        // the elevated chips so they stay visible). Only
                        // while an image can actually still arrive — a
                        // failed sheet/thumb or an unmappable timeline
                        // (unknown duration) must not keep the loop hot
                        // for as long as the bar stays open.
                        if uv.is_none() && sheet_coming && d.is_some() {
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

        // Auto-skip timer: an arc ring in the VIDEO's top-right corner
        // (the window's while nothing is decoded yet) — by default it
        // fills clockwise toward the next clip; `auto_skip_countdown`
        // flips it to a draining countdown. Only exists while the
        // countdown does — armed AND a modal view up — so turning
        // auto-skip off (or dropping back to the grid) removes it.
        // Drawn above the modal layers, below the jobs bar.
        if let Some(frac) = self.auto_skip_progress() {
            let r = self.tuning.auto_skip_ring_radius.max(4.0);
            let (cx, cy) = match self.active_video_rect() {
                Some((x, y, w, _)) => (
                    x + w - r - AUTO_SKIP_RING_MARGIN,
                    y + r + AUTO_SKIP_RING_MARGIN,
                ),
                None => (
                    self.viewport.width - r - AUTO_SKIP_RING_MARGIN,
                    r + AUTO_SKIP_RING_MARGIN,
                ),
            };
            push_countdown_ring(&mut tiles, cx, cy, r, frac, self.tuning.auto_skip_countdown);
            // The ring is a clock: keep frames coming while it's on
            // screen (live-video pacing usually covers this; levels
            // without live video need the nudge).
            self.wake(0.3);
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
                let op = t.jobs_bar_opacity.clamp(0.0, 1.0);
                let (bw, bh) = (t.jobs_bar_width.max(8.0), t.jobs_bar_height.max(1.0));
                let bx = self.viewport.width - bw - 24.0;
                let by = self.viewport.height - bh - 22.0;
                let bar = |x: f32, w: f32, a: f32| Tile {
                    x,
                    y: by,
                    w,
                    h: bh,
                    color: [0.85, 0.85, 0.9, a * op * fade],
                    border_color: [0.0; 4],
                    corner_radius: bh * 0.5,
                    border_width: 0.0,
                    uv: [0.0; 4],
                    uv2: [0.0; 4],
                    frame_fade: 0.0,
                    tex_mix: 0.0,
                    hires: false,
                    pie: 0.0,
                };
                tiles.push(bar(bx, bw, 0.12)); // track
                tiles.push(bar(bx, (bw * progress).max(bh), 0.9)); // fill
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
                    // freely under the gesture and (strip_scroll_selects,
                    // the default) the selection commits to the nearest
                    // chip — the snap spring then centers it, so it reads
                    // as magnetic, chip-by-chip flow. With it off the
                    // scroll is a PEEK: the strip pans while the selected
                    // clip keeps playing, and only a chip click (or a
                    // keyboard move) changes the selection. The grid
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
                        if self.tuning.strip_scroll_selects {
                            let i = self.strip_target.round() as usize;
                            if i != self.selected {
                                self.selected = i;
                                self.sel_changed_at = Instant::now();
                                self.pending_reselect = None; // scroll outranks the D reselect
                                self.scroll_to_selected();
                            }
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
                // In a modal the pinch steps the view-depth ladder instead
                // of zooming the (frozen/hidden) grid: pinch-in (fingers
                // apart, delta > 0) dives deeper, pinch-out (fingers
                // together) backs out. Deltas accumulate so a deliberate
                // gesture crosses MODAL_PINCH_STEP; a reversal resets it.
                if self.quickview || self.fullview {
                    if self.modal_pinch_accum != 0.0
                        && self.modal_pinch_accum.signum() != delta.signum()
                    {
                        self.modal_pinch_accum = 0.0;
                    }
                    self.modal_pinch_accum += delta;
                    if self.modal_pinch_accum >= MODAL_PINCH_STEP {
                        // Pinch-in: quickview → fullview (fullview is the floor).
                        self.modal_pinch_accum = 0.0;
                        if self.quickview && !self.fullview {
                            self.fullview = true;
                            self.wake(0.3);
                        }
                    } else if self.modal_pinch_accum <= -MODAL_PINCH_STEP {
                        // Pinch-out: fullview → strip, or strip → grid.
                        self.modal_pinch_accum = 0.0;
                        if self.fullview {
                            self.to_quickview();
                        } else if self.quickview {
                            self.quickview = false;
                            self.wake(0.3);
                        }
                    }
                    return;
                }
                self.modal_pinch_accum = 0.0;
                let factor = 1.0 + delta * self.tuning.pinch_sensitivity;
                // Ribbon mode: the pinch grips the strip and drives the
                // zoom directly — no spring, no scroll re-anchor. The
                // gripped chip stays under the fingers, so there is
                // nothing left for a camera anchor to correct.
                if self.ribbon_on() {
                    if self.ribbon.is_none() {
                        // A pinch arriving mid-settle takes over from where
                        // the morph got to, rather than fighting it.
                        if let Some(s) = self.ribbon_settle.take() {
                            self.ribbon_install(s.pack.starts, s.pack.zoom, Some(s.scroll));
                        }
                        // Only the grip needs a layout, and only once per
                        // gesture: every pinch event moves the zoom, so a
                        // layout() here is a guaranteed O(library) rebuild.
                        let lay = self.layout();
                        self.ribbon = self.ribbon_grip(&lay);
                    }
                    if self.ribbon.is_some() {
                        let t = &self.tuning;
                        let z = (self.zoom * factor.max(0.01)).clamp(t.zoom_min, t.zoom_max);
                        // Only a pinch that actually MOVES the zoom keeps
                        // the gesture alive. macOS delivers zero-delta
                        // magnification events around a gesture (and at the
                        // zoom clamps every event is a no-op), and letting
                        // those refresh the timer left the gesture — and
                        // the ribbon layout it draws — live indefinitely.
                        if (z - self.zoom).abs() > 1e-6 {
                            self.zoom = z;
                            self.zoom_target = z;
                            if let Some(g) = &mut self.ribbon {
                                g.last = Instant::now();
                            }
                        }
                        self.motion = true;
                        return;
                    }
                }
                self.set_zoom(self.zoom_target * factor.max(0.01));
            }
            InputEvent::CursorMoved { x, y } => {
                // Real pointer travel claims the attention lane (winit can
                // deliver a same-position move on focus/entry — not intent).
                if (x, y) != self.cursor {
                    self.mouse_attention = true;
                }
                self.cursor = (x, y);
                // A pressed tile/chip pulled past the threshold stops
                // being a click and leaves the app as a native drag-out.
                // The window layer needs the command NOW (inside this
                // pointer callback — the OS event seeds the session), so
                // it drains commands after CursorMoved too.
                if let Some(p) = &self.press {
                    let (dx, dy) = (x - p.x, y - p.y);
                    let t = self.tuning.drag_threshold.max(1.0);
                    if dx * dx + dy * dy >= t * t {
                        if let Some(c) = self.clips.get(p.clip) {
                            let image = if c.cloud {
                                None
                            } else {
                                self.media.cached_thumb_path(&c.path)
                            };
                            self.cmds.push(WindowCommand::BeginDrag {
                                path: c.path.clone(),
                                image,
                            });
                        }
                        // The OS owns the gesture from here: no MouseUp
                        // may ever arrive, so the press ends now.
                        self.press = None;
                    }
                }
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
                // Dock-style reveal: resting the pointer near the bottom
                // edge in fullview slides the chapter bar up as a peek (it
                // slides back down once the pointer leaves and the peek
                // window elapses). Never mid-scrub. A sticky `g`-open bar
                // is left alone — open_chapter_bar won't downgrade it.
                let reveal = self.tuning.chapter_dock_reveal_px;
                if self.fullview && !self.scrubbing && reveal > 0.0 && y >= self.viewport.height - reveal
                {
                    self.open_chapter_bar(true);
                }
            }
            InputEvent::Focus { focused } => {
                self.focused = focused;
                // Losing focus ends a pinch: the fingers are gone, and a
                // gesture left live would hold the grid in its mid-flight
                // ribbon form (and keep the loop hot) for as long as the
                // user is away — an occluded window still runs frames.
                if !focused && self.ribbon.is_some() {
                    self.ribbon_release();
                }
            }
            InputEvent::MouseDown { x, y, mods } => {
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
                // clicking the VIDEO promotes to fullview (its stream is
                // already playing — zero handoff); clicking the frosted
                // background returns to the grid.
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
                        // Chips are drag sources too (select on press,
                        // as before — selecting what you drag is right).
                        self.press = Some(Press {
                            x,
                            y,
                            clip: i,
                            open_quickview: false,
                        });
                    } else if self.quickview_video_rect().is_some_and(|(vx, vy, vw, vh)| {
                        (vx..=vx + vw).contains(&x) && (vy..=vy + vh).contains(&y)
                    }) {
                        // Click on the video itself: step up to fullview.
                        self.fullview = true;
                    } else {
                        self.quickview = false;
                    }
                    return;
                }
                let lay = self.layout();
                if let Some(i) = self.tile_at(&lay, x, y) {
                    // Attention mode: cmd-click toggles, shift-click
                    // range-selects from the selection — border-only
                    // marks that never play and never open a modal.
                    if self.attention() && (mods.cmd || mods.shift) {
                        if mods.shift {
                            let (a, b) = (self.selected.min(i), self.selected.max(i));
                            self.marked.extend(a..=b);
                        } else if !self.marked.insert(i) {
                            self.marked.remove(&i);
                        }
                        return;
                    }
                    let was_selected = i == self.selected;
                    if !was_selected {
                        self.selected = i;
                        self.sel_changed_at = Instant::now();
                        self.pending_reselect = None; // click outranks the D reselect
                    }
                    // Quickview waits for release: this press may be the
                    // start of a drag-out, not a click. Classic opens
                    // only from a click on the already-selected tile;
                    // attention opens from a single click on ANY tile,
                    // promoting the attention lane's running stream.
                    self.press = Some(Press {
                        x,
                        y,
                        clip: i,
                        open_quickview: self.attention() || was_selected,
                    });
                }
            }
            InputEvent::MouseUp { x, .. } => {
                if let Some(p) = self.press.take()
                    && p.open_quickview
                    && p.clip == self.selected
                {
                    // The press never became a drag: it was a click on
                    // the selection, which opens quickview on release.
                    self.quickview = true;
                    self.quickview_at = Instant::now();
                    self.strip_pos = self.selected as f32;
                    self.strip_target = self.strip_pos;
                }
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
        self.probe
            .counters
            .frames
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            // The chapter bar takes the played clip's true aspect. If its
            // static thumb never went atlas-resident (jumped here via
            // random/shuffle) `Clip.aspect` is still None, so resolve it
            // from the probe meta once — a one-off memoized read, so the
            // bar opens with the right chip shape instead of a 16:9
            // default. Levels without live video (no spawn to seed
            // `meta_cache`) read it straight off disk here.
            self.resolve_selected_aspect();
        }
        self.tick_auto_skip();
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
        // settled, no live video, no fade in flight — the loop drops to
        // a slow tick. Pending background jobs and an open ingest
        // producer deliberately do NOT keep the loop hot (P0.2): their
        // worker threads fire `waker` per delivery, so each completion
        // repaints once, and the idle tick still services them at 10Hz
        // as a safety net.
        if log::log_enabled!(log::Level::Debug) {
            // Atlas sizing evidence (P0.1/P0.5): actual occupancy vs the
            // zone's demand — a static per in-zone clip, plus the
            // live/hover lanes.
            let used = self.slots.iter().filter(|s| s.is_some()).count();
            let (first, last) = self.visible_rows(&lay, PREFETCH_ROWS);
            let demand = ((last - first + 1) * lay.cols).min(self.clips.len()) + 2;
            self.redraw_stats.slots(used, demand);
        }
        let motion = self.motion;
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
        frame.animating = motion || transition || timer;
        self.redraw_stats.record(
            motion,
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
        pie: 0.0,
    };
    out.push(part(bx + 2.0, by + 4.0, 13.0, 13.0)); // small bump
    out.push(part(bx + 9.0, by, 16.0, 16.0)); // big bump
    out.push(part(bx, by + 7.0, 28.0, 10.0)); // base bar
}

/// The auto-skip timer: a stroked arc ring, macOS-style — pure white,
/// opacity does all the work. A soft dark scrim disc sits behind (a
/// shadow, not a drawn border, so white-on-white stays legible), a
/// ghosted full ring is the track, and a brighter arc shows progress:
/// count-up (default) grows clockwise from 12 as `frac` (elapsed, 0..1)
/// rises; `countdown` flips it to a draining arc ending at 12. The arcs
/// are border-only circle tiles clipped by the shader's `Tile.pie`.
fn push_countdown_ring(out: &mut Vec<Tile>, cx: f32, cy: f32, r: f32, frac: f32, countdown: bool) {
    let stroke = (r * 0.375).max(2.0);
    let ring = |alpha: f32, pie: f32| Tile {
        x: cx - r,
        y: cy - r,
        w: r * 2.0,
        h: r * 2.0,
        color: [0.0; 4],
        border_color: [1.0, 1.0, 1.0, alpha],
        corner_radius: r,
        border_width: stroke,
        uv: [0.0; 4],
        uv2: [0.0; 4],
        frame_fade: 0.0,
        tex_mix: 0.0,
        hires: false,
        pie,
    };
    let scrim = SCRIM_PAD;
    out.push(Tile {
        x: cx - r - scrim,
        y: cy - r - scrim,
        w: (r + scrim) * 2.0,
        h: (r + scrim) * 2.0,
        color: [0.0, 0.0, 0.0, SCRIM_ALPHA],
        border_color: [0.0; 4],
        corner_radius: r + scrim,
        border_width: 0.0,
        uv: [0.0; 4],
        uv2: [0.0; 4],
        frame_fade: 0.0,
        tex_mix: 0.0,
        hires: false,
        pie: 0.0,
    });
    out.push(ring(0.2, 0.0));
    let sweep = if countdown {
        (1.0 - frac).clamp(0.0, 1.0)
    } else {
        frac.clamp(0.0, 1.0)
    };
    if sweep > 0.004 {
        // Positive pie drains toward 12; negative grows from 12.
        let pie = if countdown { sweep } else { -sweep };
        // Fully opaque arc + caps: the caps are separate discs drawn over
        // the arc's ends to round the shader's square angular cut, and any
        // alpha below 1.0 makes their overlap with the arc double-blend
        // into visible "bulbs". Opaque white over opaque white is seamless.
        out.push(ring(1.0, pie));
        // Round caps: little discs on the arc's centerline at both ends —
        // one fixed at 12, one riding the moving edge.
        let rc = r - stroke * 0.5;
        let cap = |turns: f32| {
            let a = turns * std::f32::consts::TAU;
            Tile {
                x: cx + a.sin() * rc - stroke * 0.5,
                y: cy - a.cos() * rc - stroke * 0.5,
                w: stroke,
                h: stroke,
                color: [1.0, 1.0, 1.0, 1.0],
                border_color: [0.0; 4],
                corner_radius: stroke * 0.5,
                border_width: 0.0,
                uv: [0.0; 4],
                uv2: [0.0; 4],
                frame_fade: 0.0,
                tex_mix: 0.0,
                hires: false,
                pie: 0.0,
            }
        };
        out.push(cap(0.0));
        if sweep < 0.996 {
            out.push(cap(if countdown { 1.0 - sweep } else { sweep }));
        }
    }
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
        pie: 0.0,
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
            pie: 0.0,
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

/// SB_HANDOFF_DUMP field instrumentation: with the env var set to a
/// directory, the first few frames every hover/selected lane serves are
/// written there as PPMs named with lane, generation, serve index and the
/// frame's content pts — next to a copy of the clip's cached thumbnail
/// jpg. The on-screen thumb→live handoff becomes diffable artifacts
/// (locate each PPM's true timestamp in the source with an SSIM sweep)
/// instead of a perception argument. Completely inert without the env
/// var; costs nothing in steady state (callers gate on the first serves).
fn handoff_dump(
    stem: &str,
    lane: &str,
    generation: u64,
    k: u32,
    pts: f64,
    w: u32,
    h: u32,
    rgba: &[u8],
    thumb: Option<PathBuf>,
) {
    let Some(dir) = std::env::var_os("SB_HANDOFF_DUMP") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let _ = std::fs::create_dir_all(&dir);
    let stem: String = stem
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .chars()
        .rev()
        .take(48)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if k == 0 && let Some(t) = thumb {
        let _ = std::fs::copy(&t, dir.join(format!("{stem}__thumb.jpg")));
    }
    let (w, h) = (w as usize, h as usize);
    if rgba.len() < w * h * 4 {
        return;
    }
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    ppm.reserve(w * h * 3);
    for px in rgba[..w * h * 4].chunks_exact(4) {
        ppm.extend_from_slice(&px[..3]);
    }
    let name = format!("{stem}__{lane}_g{generation}_f{k}_pts{pts:.3}.ppm");
    let _ = std::fs::write(dir.join(name), ppm);
    log::debug!("handoff dump: {lane} g{generation} f{k} pts={pts:.3}");
}

/// Reconcile the position-derived chapter index with a navigation intent
/// (`ChapterBar::nav`). Chapter seeks are keyframe seeks (no decode-forward
/// freeze), so a jump to chapter `k` lands on the nearest keyframe *before*
/// `times[k]` — the decoder sits in chapter `k-1`'s tail. Deriving the
/// playing chapter purely from position would then report `k-1`: the
/// highlight janks back and forward stepping recomputes the same base
/// forever (stuck). So while position is at `k` or its keyframe-undershoot
/// lead-in (`k-1`), the intent wins; once playback carries past into a
/// later chapter (or a move lands well before), the raw index takes over.
fn resolve_chapter(pos_idx: usize, nav: Option<usize>) -> usize {
    match nav {
        Some(k) if pos_idx == k || pos_idx + 1 == k => k,
        _ => pos_idx,
    }
}

/// The synthesized checkpoint starts themselves — k/n of the duration
/// for each of `checkpoint_count` chapters (empty under a minute).
/// Shared by the chapter bar's plan install and fullview's left/right
/// chapter stepping so the two can't disagree.
fn synth_checkpoints(d: f64) -> Vec<f64> {
    let n = checkpoint_count(d);
    (0..n).map(|k| d * k as f64 / n.max(1) as f64).collect()
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
    use sb_window::Mods;

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

    /// P0.7: live spawns resolve meta through the session memo — a memo
    /// hit must serve without touching the disk, and a stale recorded
    /// `src` queues the worker-side heal (once) instead of the old
    /// inline write on the render thread.
    #[test]
    fn clip_meta_memoizes_and_heal_meta_queues_the_src_heal() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });

        // A memoized meta serves with no disk entry existing at all.
        let path = PathBuf::from("/nonexistent/sb_app_meta_memo/clip.mp4");
        let sentinel = sb_media::Meta {
            src: path.clone(),
            duration: Some(2.0),
            width: Some(1280),
            height: Some(720),
            codec: Some("h264".into()),
            fps: Some(30.0),
            rotation: None,
            pix_fmt: Some("yuv420p".into()),
        };
        app.meta_cache.insert(path.clone(), sentinel);
        let got = app.clip_meta(&path).expect("memo hit needs no disk");
        assert_eq!(got.width, Some(1280));

        // A stale src queues exactly one background reprobe.
        let stale = sb_media::Meta {
            src: PathBuf::from("/somewhere/else.mp4"),
            ..got.clone()
        };
        app.heal_meta(Some(&stale), &path);
        assert!(app.reprobed.contains(&path), "stale src queues the heal");
        app.heal_meta(Some(&stale), &path); // deduped per session
        assert_eq!(app.reprobed.len(), 1);

        // A complete meta with the right src queues nothing.
        let other = PathBuf::from("/nonexistent/sb_app_meta_memo/other.mp4");
        let good = sb_media::Meta {
            src: other.clone(),
            ..got
        };
        app.heal_meta(Some(&good), &other);
        assert!(!app.reprobed.contains(&other));
    }

    /// `D` rebuilds the library from the selected clip's parent directory
    /// and the clip finds itself again once the listing streams in.
    #[test]
    fn open_parent_swaps_to_siblings_and_reselects() {
        let dir = std::env::temp_dir().join("sb_app_parent_swap_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["a.mp4", "b.mp4", "c.mp4"] {
            std::fs::write(dir.join(f), ingest::fake_mp4_bytes()).unwrap();
        }

        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None), // no live decoders in tests
            inputs: vec![dir.join("b.mp4")],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        app.tuning.gatekeeper = false; // fake fixtures — stage two would drop them
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

    /// A press on a tile pulled past `drag_threshold` becomes a native
    /// drag-out (`WindowCommand::BeginDrag` with the clip's path) and
    /// stops being a click — quickview must NOT open on the release.
    /// A press that stays put is still a click: quickview opens on
    /// MouseUp (moved off MouseDown exactly so drags can't open it).
    #[test]
    fn tile_drag_out_matures_and_click_still_opens_quickview() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        let lay = app.layout();
        let g = app.tuning.gap;
        let (cx, cy) = (g + lay.tile_w * 0.5, g + lay.tile_h * 0.5);
        assert_eq!(
            app.tile_at(&lay, cx, cy),
            Some(0),
            "test point misses tile 0"
        );

        // Drag: down on the selected tile, pull well past the threshold.
        app.event(InputEvent::MouseDown {
            x: cx,
            y: cy,
            mods: Mods::default(),
        });
        assert!(
            !app.cmds
                .iter()
                .any(|c| matches!(c, WindowCommand::BeginDrag { .. })),
            "the press alone must not start a drag"
        );
        let far = app.tuning.drag_threshold + 10.0;
        app.event(InputEvent::CursorMoved { x: cx + far, y: cy });
        let dragged = app.cmds.iter().find_map(|c| match c {
            WindowCommand::BeginDrag { path, .. } => Some(path.clone()),
            _ => None,
        });
        assert_eq!(
            dragged.as_deref(),
            Some(app.clips[0].path.as_path()),
            "pointer travel past drag_threshold emits BeginDrag for the pressed clip"
        );
        app.event(InputEvent::MouseUp { x: cx + far, y: cy });
        assert!(!app.quickview, "a matured drag must not open quickview");
        assert_eq!(
            app.cmds
                .iter()
                .filter(|c| matches!(c, WindowCommand::BeginDrag { .. }))
                .count(),
            1,
            "one gesture, one drag session"
        );

        // Click: down + up with sub-threshold wiggle opens quickview.
        app.cmds.clear();
        app.event(InputEvent::MouseDown {
            x: cx,
            y: cy,
            mods: Mods::default(),
        });
        app.event(InputEvent::CursorMoved { x: cx + 2.0, y: cy });
        assert!(!app.quickview, "quickview waits for the release");
        app.event(InputEvent::MouseUp { x: cx + 2.0, y: cy });
        assert!(
            app.quickview,
            "an un-dragged click on the selection quickviews"
        );
        assert!(
            !app.cmds
                .iter()
                .any(|c| matches!(c, WindowCommand::BeginDrag { .. })),
            "sub-threshold wiggle stays a click"
        );
    }

    /// Pending background jobs must not force continuous rendering: the
    /// gen sweep can run for hours on a big library while the grid is
    /// static — each completion wakes the loop via `Waker` instead
    /// (docs/perf-reviews/02-efficiency-review.md P0.2).
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
            created: None,
        })
        .unwrap();
        let _ = app.frame(0.016, vp);
        assert_eq!(app.clips.len(), before + 1, "late arrival ingested");

        // …and producer exit closes out ingest state.
        drop(tx);
        let _ = app.frame(0.016, vp);
        assert!(app.rx.is_none(), "disconnect noticed without a hot loop");
    }

    /// The continuity handoff: hover-preview a tile, let it play past the
    /// thumb anchor, then click it. The selected-lane open (warm
    /// promotion or fresh spawn) must continue from the PREVIEW's
    /// position, not restart at the anchor — the restart was the reported
    /// "video skips backward ~a GOP when it starts playing" (measured in
    /// benchmarks/scenarios/hover_then_select_handoff.toml: a 6s hover
    /// jumped 6s back on click). Fixture keyframes are 1s apart (-g 30),
    /// anchor = 10% of 8s = 0.8s → anchor keyframe 0.0; after >1.4s of
    /// preview the resumed open must land at a keyframe ≥ 1.0. Needs
    /// ffmpeg — skipped quietly when it's not on PATH.
    #[test]
    fn hover_click_continues_from_the_preview_position() {
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
        let dir = std::env::temp_dir().join("sb_app_hover_continuity_test");
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str| {
            let clip = dir.join(name);
            if !clip.exists() {
                let ok = std::process::Command::new("ffmpeg")
                    .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                    .arg("testsrc2=duration=8:size=320x180:rate=30")
                    .args([
                        "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g",
                        "30",
                    ])
                    .arg(&clip)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                assert!(ok, "failed to generate test clip");
            }
            clip
        };
        let a = mk("cont_a.mp4");
        let b = mk("cont_b.mp4");

        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::Normal),
            inputs: vec![a, b],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        assert!(
            pump_until(&mut app, |a| a.clips.len() == 2
                && matches!(a.clips[1].thumb, Thumb::Ready { .. })),
            "clip 1's thumb never became ready"
        );
        // Hover tile 1 (unselected) and skip the settle delay.
        let lay = app.layout();
        let (tx, ty, tw, th) = app.tile_rect(&lay, 1);
        let (cx, cy) = (tx + tw * 0.5, ty + th * 0.5);
        app.event(InputEvent::CursorMoved { x: cx, y: cy });
        app.hover_changed_at = Instant::now() - Duration::from_secs(2);
        assert!(
            pump_until(&mut app, |a| a
                .live_hover
                .as_ref()
                .is_some_and(|l| l.first_frame.is_some())),
            "hover preview never served"
        );
        // Let the preview play well past the anchor keyframe (wall time —
        // the decoder paces on the wall clock).
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let until = Instant::now() + Duration::from_millis(1600);
        while Instant::now() < until {
            let _ = app.frame(0.016, vp);
            std::thread::sleep(Duration::from_millis(5));
        }
        let preview_pos = app.live_hover.as_ref().unwrap().player.position();
        assert!(
            preview_pos > 1.2,
            "preview should have played past 1.2s, at {preview_pos}"
        );

        // Click the hovered tile: selects it; the selected-lane open
        // (promotion or spawn) must resume from the preview.
        app.event(InputEvent::MouseDown {
            x: cx,
            y: cy,
            mods: Mods::default(),
        });
        app.event(InputEvent::MouseUp { x: cx, y: cy });
        assert_eq!(app.selected, 1, "click selects the hovered tile");
        assert!(
            pump_until(&mut app, |a| a
                .live_sel
                .as_ref()
                .is_some_and(|l| l.clip == 1 && l.first_frame.is_some())),
            "selected stream never served after the click"
        );
        let pos = app.live_sel.as_ref().unwrap().position();
        assert!(
            pos >= 0.9,
            "selected open must continue from the preview (~{preview_pos:.1}s, keyframe ≥1.0), \
             not restart at the anchor keyframe 0.0 — got {pos}"
        );
        assert!(
            pos <= preview_pos + 1.0,
            "resumed position should be near the preview, got {pos} vs {preview_pos}"
        );
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
        // `-g 30` = a keyframe every second (30fps): skips are now keyframe
        // seeks, so a forward jump must have a keyframe to land ON, and the
        // chained-skip assertion needs the first skip to settle forward.
        // (ultrafast disables scenecut and defaults keyint to 250 > 240
        // frames — a single keyframe at t=0 would collapse every keyframe
        // seek back to 0.) The `_kf` name forces a fresh encode past any
        // cached single-keyframe `skip.mp4` from before the seek change.
        let clip = dir.join("skip_kf.mp4");
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
                    "-g",
                    "30",
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
        // The same stream delivers from the new offset (keyframe seek).
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
                created: None,
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

        // The gatekeeper (rightly) refuses this file at ingest now, so
        // install the clip directly — the subject here is the lane
        // reap + cooldown, which must still handle a stream that fails
        // async after ingest let a file through.
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::Normal), // live lanes enabled
            demo: true,                         // no stdin reader in tests
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        app.clips.clear();
        app.index.clear();
        app.reflow.clear();
        app.demo = false; // live lanes (and their cooldown) must engage
        app.clips.push(Clip {
            path: bad.clone(),
            readable: true,
            cloud: false,
            cached: false,
            spawned: Instant::now(),
            scale: 1.0,
            emph: 0.0,
            thumb: Thumb::None,
            anim: Thumb::None,
            aspect: None,
            created: None,
        });
        app.index.insert(bad.clone(), 0);
        app.selected = 0;

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
            generation: 0,
            served: 0,
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
                aspect: None,
                created: None,
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

    /// Losing window focus PARKS the selected stream (same machinery as
    /// offscreen parking) and refocus resumes the SAME decoder near
    /// where playback stopped. The old behavior reaped every live lane
    /// on focus loss, so returning paid a cold respawn that visibly
    /// jumped the video back to the thumbnail's seek-in frame. Needs
    /// ffmpeg.
    #[test]
    fn focus_pause_parks_and_resumes_the_same_stream() {
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
        let dir = std::env::temp_dir().join("sb_app_focus_park_test");
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("focus.mp4");
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
        // Let playback advance a little past the seek-in point, so a
        // respawn (which restarts there) would be distinguishable from
        // a genuine resume.
        let start = app.live_sel.as_ref().unwrap().player.position();
        assert!(
            pump_until(&mut app, |a| a
                .live_sel
                .as_ref()
                .is_some_and(|l| l.player.position() > start + 0.3)),
            "playback never advanced"
        );

        app.event(InputEvent::Focus { focused: false });
        assert!(
            pump_until(&mut app, |a| a.sel_parked),
            "focus loss never parked the stream"
        );
        let parked_pos = app.live_sel.as_ref().unwrap().player.position();

        // Parked: the stream survives and nothing uploads.
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        for _ in 0..30 {
            let f = app.frame(0.016, vp);
            assert!(f.hires_upload.is_none(), "paused stream must not upload");
        }
        assert!(app.live_sel.is_some(), "paused stream must survive");

        // Refocus: the SAME decoder resumes near the parked position.
        app.event(InputEvent::Focus { focused: true });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut resumed = false;
        while Instant::now() < deadline {
            if app.frame(0.016, vp).hires_upload.is_some() {
                resumed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(resumed, "no frames after refocus");
        assert_eq!(
            app.live_sel.as_ref().unwrap().spawned,
            spawned0,
            "refocus must reuse the parked decoder, not respawn"
        );
        let resumed_pos = app.live_sel.as_ref().unwrap().player.position();
        assert!(
            resumed_pos >= parked_pos - 0.05 && resumed_pos < parked_pos + 1.0,
            "playback must continue from the parked position \
             (parked {parked_pos:.2}s, resumed {resumed_pos:.2}s)"
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
            mods: Mods::default(),
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
            std::fs::write(dir.join(f), ingest::fake_mp4_bytes()).unwrap();
        }
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None), // no live decoders in tests
            inputs: vec![dir.clone()],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        app.tuning.gatekeeper = false; // fake fixtures — stage two would drop them
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

    /// In quickview, a click on the video steps up to fullview (its
    /// stream is already playing — zero handoff); a click on the frosted
    /// background returns to the grid.
    #[test]
    fn quickview_click_video_enters_fullview_background_exits() {
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
        let _ = app.frame(0.016, vp); // sets the viewport
        // quickview_video_rect needs a Ready thumb (its aspect sizes the
        // rect); demo tiles start empty, so install one.
        app.clips[0].thumb = Thumb::Ready {
            slot: 0,
            at: Instant::now(),
            tw: 640,
            th: 360,
        };
        app.selected = 0;
        app.quickview = true;
        app.quickview_at = Instant::now();

        let (vx, vy, vw, vh) = app.quickview_video_rect().expect("video rect");
        // Click dead center of the video: step up to fullview.
        app.event(InputEvent::MouseDown {
            x: vx + vw * 0.5,
            y: vy + vh * 0.5,
            mods: Mods::default(),
        });
        assert!(app.fullview, "clicking the video enters fullview");
        assert!(
            app.quickview,
            "fullview layers over quickview, not replacing it"
        );

        // Back to quickview, then click the frosted background (top-left
        // corner, well outside the centered video) → return to the grid.
        app.fullview = false;
        app.event(InputEvent::MouseDown {
            x: 4.0,
            y: 4.0,
            mods: Mods::default(),
        });
        assert!(!app.quickview, "clicking the background closes to the grid");
        assert!(!app.fullview, "and does not enter fullview");
    }

    /// With strip_scroll_selects off, scrolling the filmstrip only
    /// peeks: the strip pans, the selection stays put (the video keeps
    /// playing), a chip click selects, and the strip re-centers on the
    /// new selection instead of holding the peek.
    #[test]
    fn filmstrip_scroll_peeks_without_selecting() {
        let dir = std::env::temp_dir().join("sb_app_strip_peek_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["a.mp4", "b.mp4", "c.mp4"] {
            std::fs::write(dir.join(f), ingest::fake_mp4_bytes()).unwrap();
        }
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None), // no live decoders in tests
            inputs: vec![dir.clone()],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        app.tuning.gatekeeper = false; // fake fixtures — stage two would drop them
        assert!(pump_until(&mut app, |a| a.clips.len() == 3), "ingest");
        app.tuning.strip_scroll_selects = false;
        app.quickview = true;
        app.quickview_at = Instant::now();
        app.strip_pos = 0.0;
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };

        let (cw, _, _) = app.strip_geom();
        let step = cw + app.tuning.strip_gap;
        app.event(InputEvent::Scroll {
            dx: -step * 2.0,
            dy: 0.0,
        });
        assert_eq!(app.selected, 0, "peeking never moves the selection");
        assert!(
            app.strip_target > 1.5,
            "the strip itself panned to the peeked chips"
        );
        // The peek holds: settle time passes and the strip must NOT home
        // back onto the (unchanged) selection.
        std::thread::sleep(Duration::from_millis(140));
        let _ = app.frame(0.016, vp);
        assert!(
            app.strip_target > 1.5,
            "a peeked strip holds until the selection moves"
        );

        // Clicking the peeked chip selects it, and the strip re-centers.
        let (_, ch, sy) = app.strip_geom();
        let hit = app
            .strip_layout(app.strip_pos)
            .into_iter()
            .find(|&(i, _, _)| i == 2)
            .expect("peeked chip on screen");
        app.event(InputEvent::MouseDown {
            x: hit.1,
            y: sy + ch * 0.5,
            mods: Mods::default(),
        });
        assert_eq!(app.selected, 2, "a chip click selects the peeked clip");
        std::thread::sleep(Duration::from_millis(140));
        let _ = app.frame(0.016, vp);
        assert!(
            (app.strip_target - 2.0).abs() < 0.01,
            "the strip homes onto the new selection"
        );
    }

    /// Backdrop styles: a flat quickview never builds (or freezes) the
    /// grid behind its opaque stage, while a blurred fullview does —
    /// each modal can wear either backdrop.
    #[test]
    fn backdrop_styles_gate_the_grid() {
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
        let _ = app.frame(0.016, vp);

        app.tuning.quickview_backdrop = BackdropStyle::Flat;
        app.quickview = true;
        app.quickview_at = Instant::now();
        let _ = app.frame(0.016, vp);
        assert!(
            app.frozen_grid.is_none(),
            "flat quickview: the hidden grid is never built or frozen"
        );

        // Same session, fullview goes tinted: the grid IS the backdrop
        // again, so the freeze snapshot must exist.
        app.tuning.fullview_backdrop = BackdropStyle::Blur;
        app.fullview = true;
        let _ = app.frame(0.016, vp);
        assert!(
            app.frozen_grid.is_some(),
            "blurred fullview: the frozen grid backs the frost"
        );
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
        // Multi-select marks (attention mode) are index-keyed too.
        app.marked = [2usize, 9, 20].into_iter().collect();
        let marked_paths: std::collections::HashSet<PathBuf> = app
            .marked
            .iter()
            .map(|&i| app.clips[i].path.clone())
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
        let marked_now: std::collections::HashSet<PathBuf> = app
            .marked
            .iter()
            .map(|&i| app.clips[i].path.clone())
            .collect();
        assert_eq!(marked_now, marked_paths, "marks follow their clips");
    }

    /// Sorted ingest (`--sort newest`): arrivals merge into a creation-
    /// date-sorted grid AS THEY STREAM — a later batch carrying a newer
    /// file inserts ahead of already-landed tiles (one stable merge + one
    /// remap per frame, never per item), the path→index map stays
    /// consistent, and the selection follows its clip through the remap.
    #[test]
    fn sorted_ingest_places_arrivals_by_created_date() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            sort: Some(SortMode::Newest),
            ..Options::default()
        });
        // Start from an empty library; the injected channel is the producer.
        app.clips.clear();
        app.index.clear();
        app.reflow.clear();
        app.selected = 0;
        let (tx, rx) = std::sync::mpsc::channel();
        app.rx = Some(rx);
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let t0 = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let send = |name: &str, secs: u64| {
            tx.send(ingest::Ingested {
                path: PathBuf::from(name),
                readable: false, // no media jobs in this test
                cloud: false,
                created: Some(t0 + Duration::from_secs(secs)),
            })
            .unwrap();
        };
        send("sorted/a.mp4", 100);
        let _ = app.frame(0.016, vp);
        assert_eq!(app.clips.len(), 1);
        assert_eq!(app.clips[app.selected].path, PathBuf::from("sorted/a.mp4"));

        // A newer and an older file land in one later batch: the newer
        // inserts BEFORE the already-landed clip, the older after it.
        send("sorted/b.mp4", 300);
        send("sorted/c.mp4", 50);
        let _ = app.frame(0.016, vp);
        let order: Vec<String> = app
            .clips
            .iter()
            .map(|c| c.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(order, ["b.mp4", "a.mp4", "c.mp4"], "newest first");
        for (i, c) in app.clips.iter().enumerate() {
            assert_eq!(
                app.index.get(&c.path),
                Some(&i),
                "path→index stays consistent through the merge remap"
            );
        }
        assert_eq!(
            app.clips[app.selected].path,
            PathBuf::from("sorted/a.mp4"),
            "the selection follows its clip as newer arrivals push it down"
        );

        // A shuffle takes over the arrangement: later arrivals append.
        app.clips.iter_mut().for_each(|c| c.cached = true);
        app.shuffle_library();
        send("sorted/d.mp4", 400);
        let _ = app.frame(0.016, vp);
        assert_eq!(
            app.clips.last().unwrap().path,
            PathBuf::from("sorted/d.mp4"),
            "post-shuffle arrivals append instead of re-sorting the shuffle"
        );
    }

    /// Gatekeeper stage two: a file wearing a valid extension AND a
    /// plausible container header, but with no decodable video behind it
    /// (truncated download, corrupt body), is dropped from the grid when
    /// its gen-sweep job proves it unplayable — the real clip beside it
    /// survives, indices intact. Needs ffmpeg — skipped quietly when it's
    /// not on PATH.
    #[test]
    fn unplayable_clip_is_removed_from_the_grid() {
        let have_ffmpeg = std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !have_ffmpeg {
            return;
        }
        let dir = std::env::temp_dir().join("sb_app_gatekeeper_stage2_test");
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("gate_good.mp4");
        if !good.exists() {
            let ok = std::process::Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=1:size=320x180:rate=30")
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
                .arg(&good)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to generate test clip");
        }
        // Passes the header sniff (real ftyp box), fails real decoding.
        let bad = dir.join("gate_bad.mp4");
        std::fs::write(&bad, ingest::fake_mp4_bytes()).unwrap();

        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None), // no live decoders in tests
            inputs: vec![good.clone(), bad.clone()],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        assert!(
            pump_until(&mut app, |a| {
                a.rx.is_none() && a.clips.len() == 1 && a.jobs_done >= a.jobs_total
            }),
            "the unplayable clip leaves the grid once the sweep judges it"
        );
        assert_eq!(app.clips[0].path, good, "the real clip survives");
        assert_eq!(app.index.get(&good), Some(&0), "indices remapped");
        assert!(app.index.get(&bad).is_none(), "the dropped path unindexed");
    }

    /// Auto-skip advances the selection once the current clip's time is
    /// up, wraps at the end of the library, only runs while a modal view
    /// (quickview/fullview) is up, and shows a timer ring exactly
    /// while the countdown exists.
    #[test]
    fn auto_skip_advances_and_wraps() {
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
        app.tuning.auto_skip_s = 5.0;

        // Off: an expired countdown does nothing, no ring.
        app.quickview = true;
        app.sel_changed_at = expired;
        let _ = app.frame(0.016, vp);
        assert_eq!(app.selected, 0, "auto-skip off: selection holds");
        assert_eq!(app.auto_skip_progress(), None, "off: no timer ring");

        // Armed but back in the grid: the countdown suspends (and keeps
        // re-anchoring, so it can't fire the moment a modal reopens).
        app.quickview = false;
        app.auto_skip_since = Some(expired);
        app.sel_changed_at = expired;
        let _ = app.frame(0.016, vp);
        assert_eq!(app.selected, 0, "grid view: countdown suspended");
        assert_eq!(app.auto_skip_progress(), None, "grid view: ring hidden");
        assert!(
            app.auto_skip_since.unwrap() > expired,
            "suspension re-anchors the countdown"
        );

        // In quickview it advances…
        app.quickview = true;
        app.auto_skip_since = Some(expired);
        app.sel_changed_at = expired;
        assert_eq!(
            app.auto_skip_progress(),
            Some(1.0),
            "modal + armed: ring shows a spent countdown"
        );
        let _ = app.frame(0.016, vp);
        assert_eq!(app.selected, 1, "auto-skip on: selection advances");
        // …and re-arms (the new clip gets its own countdown, ring reset).
        let _ = app.frame(0.016, vp);
        assert_eq!(app.selected, 1, "fresh selection: countdown restarts");
        assert!(
            app.auto_skip_progress().unwrap() < 1.0,
            "fresh selection: the ring starts over"
        );

        // Wraps from the last clip back to the first (fullview counts too).
        app.quickview = false;
        app.fullview = true;
        app.selected = app.clips.len() - 1;
        app.auto_skip_since = Some(expired);
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
                peek_until: None,
                nav: None,
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

    /// A fullview chapter step reveals the bar as a timed peek that slides
    /// itself back down once its window elapses; a deliberate `g` open has
    /// no deadline and stays up.
    #[test]
    fn chapter_peek_auto_closes_but_g_open_stays() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None), // snap-mode slide lands fast
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        app.fullview = true;
        let bar = |app: &Switchblade, peek_until| ChapterBar {
            path: app.clips[app.selected].path.clone(),
            duration: Some(400.0),
            times: Some(vec![0.0, 50.0, 100.0]),
            open: true,
            slide: 1.0,
            pos: 0.0,
            target: 0.0,
            peek_until,
            nav: None,
        };

        // An already-elapsed peek starts closing on the next frame, then
        // the slide lands and the state drops.
        app.chapters = Some(bar(&app, Some(Instant::now() - Duration::from_secs(1))));
        for _ in 0..4 {
            let _ = app.frame(0.016, vp);
        }
        assert!(app.chapters.is_none(), "an expired peek slides down and drops");

        // A sticky (g-opened) bar has no deadline — it never auto-closes.
        app.chapters = Some(bar(&app, None));
        for _ in 0..6 {
            let _ = app.frame(0.016, vp);
        }
        assert!(
            app.chapters.as_ref().is_some_and(|b| b.open),
            "a deliberate g open never auto-closes"
        );
    }

    /// Vertical keys are a view-depth ladder: up dives quickview →
    /// fullview (the strip staying live underneath), down steps fullview →
    /// quickview → grid. The ladder's top is a dead-end: up in fullview
    /// (with no chapter bar up) does nothing (no fall-through to a
    /// selection move).
    #[test]
    fn vertical_keys_step_between_quickview_and_fullview() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });

        // Quickview ("stripview"): up dives into fullview, quickview
        // staying true underneath (fullview layers over it).
        app.key(Key::Space);
        assert!(app.quickview && !app.fullview, "space opens quickview");
        // Down in quickview drops back to the grid.
        app.key(Key::Down);
        assert!(
            !app.quickview && !app.fullview,
            "down in quickview returns to the grid"
        );
        // Re-open and go up into fullview.
        app.key(Key::Space);
        app.key(Key::Up);
        assert!(app.fullview, "up dives into fullview");
        assert!(app.quickview, "quickview stays under fullview");

        // Up in fullview (no chapter bar) is a dead-end too (the ceiling).
        let sel = app.selected;
        app.key(Key::Up);
        assert!(
            app.fullview && app.selected == sel,
            "up in fullview does nothing when no chapter bar is up"
        );

        // Down steps back out to the filmstrip quickview.
        app.key(Key::Down);
        assert!(!app.fullview, "down leaves fullview");
        assert!(app.quickview, "down lands on the filmstrip quickview");

        // Fullview entered directly (no quickview under it): down still
        // brings the strip up rather than dropping to the grid.
        app.key(Key::Escape);
        assert!(!app.quickview && !app.fullview, "esc returns to the grid");
        app.key(Key::Tab);
        assert!(app.fullview && !app.quickview, "tab enters fullview only");
        app.key(Key::Down);
        assert!(
            !app.fullview && app.quickview,
            "down opens the filmstrip quickview from a direct fullview"
        );
    }

    /// In fullview, up dismisses the chapter bar (swiping it down off
    /// screen) when one is up, instead of hitting the ceiling dead-end.
    #[test]
    fn fullview_up_dismisses_the_chapter_bar() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true,
            ..Options::default()
        });
        app.key(Key::Tab);
        assert!(app.fullview, "tab enters fullview");
        // Build a sticky (g-style) bar directly — open_chapter_bar bails in
        // demo mode (no readable file), so mirror the sibling chapter tests.
        app.chapters = Some(ChapterBar {
            path: app.clips[app.selected].path.clone(),
            duration: Some(400.0),
            times: Some(vec![0.0, 50.0, 100.0]),
            open: true,
            slide: 1.0,
            pos: 0.0,
            target: 0.0,
            peek_until: None,
            nav: None,
        });
        app.key(Key::Up);
        assert!(
            app.chapters.as_ref().map(|b| !b.open).unwrap_or(true),
            "up sends the chapter bar back down"
        );
        assert!(app.fullview, "fullview stays — only the bar was dismissed");
    }

    /// A pinch in a modal steps the view-depth ladder instead of zooming
    /// the hidden grid: pinch-in (fingers apart) dives deeper, pinch-out
    /// (fingers together) backs out. Deltas accumulate past MODAL_PINCH_STEP.
    #[test]
    fn modal_pinch_steps_the_view_ladder() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true,
            ..Options::default()
        });
        let big = MODAL_PINCH_STEP + 0.01;

        // Quickview: pinch-in → fullview.
        app.key(Key::Space);
        assert!(app.quickview && !app.fullview, "space opens quickview");
        app.event(InputEvent::Pinch { delta: big });
        assert!(app.fullview, "pinch-in from quickview dives into fullview");

        // Fullview: pinch-out → strip (quickview).
        app.event(InputEvent::Pinch { delta: -big });
        assert!(
            !app.fullview && app.quickview,
            "pinch-out from fullview backs out to the strip"
        );

        // Strip: pinch-out → grid.
        app.event(InputEvent::Pinch { delta: -big });
        assert!(
            !app.quickview && !app.fullview,
            "pinch-out from the strip returns to the grid"
        );

        // A tiny pinch under the threshold does nothing.
        app.key(Key::Space);
        app.event(InputEvent::Pinch {
            delta: MODAL_PINCH_STEP * 0.3,
        });
        assert!(
            app.quickview && !app.fullview,
            "a sub-threshold pinch is inert"
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
        std::fs::write(dir.join("a.mp4"), ingest::fake_mp4_bytes()).unwrap();
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::Normal), // live video expected
            inputs: vec![dir.join("a.mp4")],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        app.tuning.gatekeeper = false; // fake fixtures — stage two would drop them
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

    /// A clip reached without its static thumb atlas-resident (jumped to
    /// via random/shuffle, or the chapter bar opened before the thumb
    /// landed) still gets its true shape: `chip_aspect` falls back to the
    /// probe meta (rotation applied), and fullview pins `Clip.aspect` so
    /// the chapter bar never shows a 16:9-default rectangle for a
    /// portrait clip.
    #[test]
    fn chapter_chip_aspect_falls_back_to_meta() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        let (_, ch, _) = app.strip_geom();
        // Clip 0: no atlas thumb, no persisted aspect — exactly the
        // off-screen jump target the bug hit.
        app.clips[0].thumb = Thumb::None;
        app.clips[0].aspect = None;
        let path = app.clips[0].path.clone();
        let portrait = sb_media::Meta {
            src: path.clone(),
            duration: Some(120.0),
            width: Some(1080),
            height: Some(1920),
            codec: Some("h264".into()),
            fps: Some(30.0),
            rotation: None,
            pix_fmt: Some("yuv420p".into()),
        };
        app.meta_cache.insert(path.clone(), portrait.clone());

        // Without the fallback this returned the 16:9 default; now it
        // reads the meta's true portrait shape.
        app.selected = 0;
        assert!(
            (app.chip_aspect(0) - 9.0 / 16.0).abs() < 0.01,
            "meta fallback gives the portrait shape, got {}",
            app.chip_aspect(0)
        );
        assert!(
            (app.chapter_chip_w() - ch * 9.0 / 16.0).abs() < 0.5,
            "chapter chips take the portrait shape from meta"
        );

        // A 90° rotation swaps the coded landscape dims to a portrait
        // display shape.
        let rotated = sb_media::Meta {
            width: Some(1920),
            height: Some(1080),
            rotation: Some(90.0),
            ..portrait.clone()
        };
        assert!(
            (Switchblade::meta_aspect_of(&rotated).unwrap() - 9.0 / 16.0).abs() < 0.01,
            "±90° rotation swaps to a portrait display aspect"
        );
        let neg = sb_media::Meta {
            rotation: Some(-90.0),
            ..rotated.clone()
        };
        assert!(
            (Switchblade::meta_aspect_of(&neg).unwrap() - 9.0 / 16.0).abs() < 0.01,
            "signed rotation also swaps"
        );
        let half = sb_media::Meta {
            rotation: Some(180.0),
            ..rotated.clone()
        };
        assert!(
            (Switchblade::meta_aspect_of(&half).unwrap() - 16.0 / 9.0).abs() < 0.01,
            "180° keeps the landscape shape"
        );

        // Fullview pins the aspect onto the clip (one resolve), so it
        // survives even after the meta memo is cleared.
        app.fullview = true;
        app.resolve_selected_aspect();
        assert_eq!(
            app.clips[0].aspect,
            Some(1080.0 / 1920.0),
            "fullview pins Clip.aspect from meta"
        );
        app.meta_cache.clear();
        assert!(
            (app.chip_aspect(0) - 9.0 / 16.0).abs() < 0.01,
            "pinned aspect outlives the meta memo"
        );
    }

    /// The flexible (default) grid: rows fill with true-aspect tiles at
    /// a shared per-row height, then justify — each row's height flexes
    /// (within the row_height caps) so it spans the viewport width; the
    /// last row stays at nominal height. Hit-tests follow the variable
    /// rects, and "fixed" restores the uniform grid.
    #[test]
    fn flexible_rows_justify_true_aspect_tiles() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        assert_eq!(
            app.tuning.grid_layout,
            GridStyle::Flexible,
            "flexible is the default layout"
        );
        let lay = app.layout();
        let flex = lay.flex.clone().expect("flexible mode builds rows");
        assert!(flex.rows.len() > 1, "the demo library spans several rows");
        let g = app.tuning.gap;
        let vw = app.viewport.width;
        // Demo runs at zoom 1.0: nominal row height = the tuning tile.
        let nominal_h = app.tuning.tile_height;
        let hmin = nominal_h * app.tuning.row_height_min;
        let hmax = nominal_h * app.tuning.row_height_max;
        for (k, row) in flex.rows.iter().enumerate() {
            let last = k + 1 == flex.rows.len();
            for i in row.start..row.end {
                let (_, y, w, h) = app.tile_rect(&lay, i);
                assert!((y - row.y).abs() < 0.01 && (h - row.h).abs() < 0.01);
                assert!(
                    (w - app.chip_aspect(i) * row.h).abs() < 0.5,
                    "tile {i} keeps its clip's aspect at the row height"
                );
            }
            let right = row.x.last().unwrap() + row.w.last().unwrap();
            assert!(right <= vw - g + 0.5, "row {k} never overflows the width");
            if last {
                assert!(row.h <= nominal_h + 0.5, "the last row never grows");
                continue;
            }
            assert!(
                row.h >= hmin - 0.5 && row.h <= hmax + 0.5,
                "row {k} height {} stays within the caps",
                row.h
            );
            // Justified: unless a cap kicked in, the row spans the width.
            let capped = row.h <= hmin + 0.5 || row.h >= hmax - 0.5;
            if !capped {
                assert!(
                    (right - (vw - g)).abs() < 0.5,
                    "row {k} right edge {right} spans the viewport"
                );
            }
        }
        // Portrait clips get narrower tiles, never taller: demo clip 1 is
        // 9:16 in the same row as 16:9 clip 0.
        let (_, y0, w0, h0) = app.tile_rect(&lay, 0);
        let (_, y1, w1, h1) = app.tile_rect(&lay, 1);
        assert!((y0 - y1).abs() < 0.01 && (h0 - h1).abs() < 0.01);
        assert!(w1 < w0 * 0.5, "the portrait tile is narrower");
        // Hit-tests agree with the rects.
        for i in [0usize, 1, 7, 20, 33] {
            let (x, y, w, h) = app.tile_rect(&lay, i);
            assert_eq!(
                app.tile_at(&lay, x + w * 0.5, y + h * 0.5 - app.scroll),
                Some(i),
                "tile_at finds tile {i} at its center"
            );
        }
        // An aspect arrival reflows: the memoized grid rebuilds.
        app.clips[0].aspect = Some(1.0);
        app.grid_rev = app.grid_rev.wrapping_add(1);
        let lay2 = app.layout();
        let (_, _, w0b, h0b) = app.tile_rect(&lay2, 0);
        assert!(
            (w0b - h0b).abs() < 0.5,
            "the square clip's tile follows its new aspect"
        );
        // "fixed" restores the uniform grid.
        app.tuning.grid_layout = GridStyle::Fixed;
        let lay3 = app.layout();
        assert!(lay3.flex.is_none(), "fixed mode has no flex rows");
        let (_, _, wf, hf) = app.tile_rect(&lay3, 1);
        assert!(
            (wf - lay3.tile_w).abs() < 0.01 && (hf - lay3.tile_h).abs() < 0.01,
            "fixed tiles are uniform regardless of aspect"
        );
    }

    /// Vertical selection moves in the flexible grid land on the
    /// horizontally nearest tile of the adjacent row — flexible rows
    /// don't share column edges, so "down" is visual, not index math.
    #[test]
    fn flexible_vertical_moves_land_on_the_nearest_tile() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        let lay = app.layout();
        app.selected = 2;
        let (x, _, w, _) = app.tile_rect(&lay, 2);
        let cx = x + w * 0.5;
        app.move_selection(0, 1);
        let lay = app.layout();
        assert_eq!(app.row_of(&lay, app.selected), 1, "moved one row down");
        assert_eq!(
            Some(app.selected),
            app.nearest_in_row(&lay, 1, cx),
            "landed on the horizontally nearest tile below"
        );
        // …and back up: the landing tile again tracks the pointer x, so
        // a down-up round trip stays in the same neighborhood.
        let (x, _, w, _) = app.tile_rect(&lay, app.selected);
        let cx = x + w * 0.5;
        app.move_selection(0, -1);
        let lay = app.layout();
        assert_eq!(app.row_of(&lay, app.selected), 0, "moved back to row 0");
        assert_eq!(Some(app.selected), app.nearest_in_row(&lay, 0, cx));
    }

    /// Build an app with a demo library, fully spawned, ready to pinch.
    #[cfg(test)]
    fn ribbon_app(vp: Viewport) -> Switchblade {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::Minimal), // ui() true, so the ribbon runs
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        let past = Instant::now() - Duration::from_secs(5);
        for c in &mut app.clips {
            c.spawned = past;
        }
        let _ = app.frame(0.016, vp);
        app
    }

    /// Rows the viewport can show at the current layout (a loose bound for
    /// "the morph didn't carry the whole library").
    #[cfg(test)]
    fn flex_rows_on_screen(app: &Switchblade) -> usize {
        let lay = app.layout();
        let (first, last) = app.visible_rows(&lay, 0);
        last - first + 1
    }

    /// The rules a ribbon frame must never break, at ANY point of a pinch
    /// or its release morph: chips in a row are joined to their neighbours
    /// by exactly one gap (so they never overlap and never leave a hole),
    /// every chip in a row is a full member of it (the row's height, its
    /// own aspect), and consecutive rows are one gap apart. A chip may
    /// appear twice — and only twice — while it straddles a boundary.
    #[cfg(test)]
    fn assert_ribbon_invariants(app: &Switchblade, rows: &[RibbonRow], what: &str) {
        let g = app.tuning.gap;
        let vw = app.viewport.width;
        for (r, row) in rows.iter().enumerate() {
            assert!(!row.items.is_empty(), "{what}: row {r} is empty");
            for w in row.items.windows(2) {
                // A copy that has left the row exits through the window
                // edge, so it is allowed to part company with its old
                // neighbour — but only once it is fully out of sight.
                let gone = |it: &RibbonItem| it.x + it.w <= 0.01 || it.x >= vw - 0.01;
                if gone(&w[0]) || gone(&w[1]) {
                    continue;
                }
                let join = w[1].x - (w[0].x + w[0].w);
                assert!(
                    (join - g).abs() < 0.05,
                    "{what}: row {r} join {join} between clips {} and {}",
                    w[0].clip,
                    w[1].clip,
                );
            }
            for it in &row.items {
                let want = app.chip_aspect(it.clip) * row.h;
                assert!(
                    (it.w - want).abs() < 0.05,
                    "{what}: row {r} clip {} is {} wide, not its aspect at the row height {want}",
                    it.clip,
                    it.w,
                );
            }
            if r > 0 {
                let vgap = row.y - (rows[r - 1].y + rows[r - 1].h);
                assert!(
                    vgap > -0.05 && vgap < g + 0.05,
                    "{what}: rows {}/{r} are {vgap} apart (overlap or dead band)",
                    r - 1,
                );
            }
        }
        let mut seen = std::collections::HashMap::new();
        for row in rows {
            for it in &row.items {
                *seen.entry(it.clip).or_insert(0) += 1;
            }
        }
        for (clip, n) in seen {
            assert!(n <= 2, "{what}: clip {clip} drawn {n}× (more than a split)");
        }
    }

    /// A pinch grips the strip at the chip under the cursor and holds it
    /// there: through a long zoom the gripped chip stays pinned under the
    /// fingers, and every frame of the gesture obeys the row rules.
    #[test]
    fn ribbon_pinch_holds_the_gripped_chip_under_the_cursor() {
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let mut app = ribbon_app(vp);
        assert!(app.ribbon_on(), "flexible + minimal animation pinches ribbon");
        app.event(InputEvent::CursorMoved { x: 640.0, y: 360.0 });
        let gripped = {
            let lay = app.layout();
            app.tile_at(&lay, 640.0, 360.0).expect("a chip at the cursor")
        };

        for k in 0..40 {
            app.event(InputEvent::Pinch { delta: 0.02 });
            let _ = app.frame(0.016, vp);
            let grip = app.ribbon.as_ref().expect("the gesture holds its grip");
            assert_eq!(grip.clip, gripped, "the grip stays on the chip it took");
            let (rows, _) = app.ribbon_walk(grip, app.zoom, false);
            assert_ribbon_invariants(&app, &rows, &format!("pinch step {k}"));
            // The gripped chip is exactly where the fingers are.
            let held = rows
                .iter()
                .find_map(|r| {
                    r.items
                        .iter()
                        .find(|it| it.clip == gripped)
                        .map(|it| (it.x + grip.fx * it.w, r.y + grip.fy * r.h))
                })
                .expect("the gripped chip is drawn");
            assert!(
                (held.0 - grip.cx).abs() < 0.05 && (held.1 - grip.cy).abs() < 0.05,
                "step {k}: gripped chip drifted to {held:?}, grip is at ({}, {})",
                grip.cx,
                grip.cy,
            );
        }
        assert!(app.zoom > 1.5, "the pinch actually zoomed: {}", app.zoom);
    }

    /// Releasing settles onto the NEAREST SAFE layout instead of slinging
    /// rows back to a justified packing: the morph obeys the row rules at
    /// every step, and at rest every clip sits in exactly one row with no
    /// chip clipped by either window edge. Rows may keep slack — that is
    /// the point: nothing is dragged sideways to left-align them.
    #[test]
    fn ribbon_release_settles_on_a_safe_layout() {
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let mut app = ribbon_app(vp);
        app.event(InputEvent::CursorMoved { x: 500.0, y: 300.0 });
        for _ in 0..30 {
            app.event(InputEvent::Pinch { delta: 0.02 });
            let _ = app.frame(0.016, vp);
        }
        // The gesture goes quiet: the next frame past the release window
        // ends it and starts the morph.
        if let Some(g) = &mut app.ribbon {
            g.last = Instant::now() - Duration::from_millis(400);
        }
        let _ = app.frame(0.016, vp);
        assert!(app.ribbon.is_none(), "the quiet gesture ended");
        let settle = app.ribbon_settle.as_ref().expect("a release morph runs");
        assert_eq!(
            settle.from.len(),
            settle.to.len(),
            "the morph maps rows one to one"
        );
        assert!(
            settle.from.len() <= flex_rows_on_screen(&app) + 2,
            "the morph only carries the rows on screen"
        );
        for step in 0..60 {
            let rows = app.ribbon_rows().expect("morph rows");
            assert_ribbon_invariants(&app, &rows, &format!("settle step {step}"));
            let _ = app.frame(0.016, vp);
            if app.ribbon_settle.is_none() {
                break;
            }
        }
        assert!(app.ribbon_settle.is_none(), "the morph landed");
        let pack = app.ribbon_pack.as_ref().expect("its packing was installed");
        assert_eq!(pack.starts.first(), Some(&0), "the packing covers clip 0");
        assert!(
            pack.starts.windows(2).all(|w| w[0] < w[1]),
            "rows partition the library in order"
        );

        // At rest: one row each, nothing clipped, rows still joined.
        let lay = app.layout();
        let flex = lay.flex.clone().expect("the packing is the layout now");
        let g = app.tuning.gap;
        let mut count = vec![0u32; app.clips.len()];
        for row in &flex.rows {
            for i in row.start..row.end {
                count[i] += 1;
                let (x, _, w, _) = app.tile_rect(&lay, i);
                assert!(
                    x >= g - 0.05 && x + w <= vp.width - g + 0.05,
                    "clip {i} is clipped by a window edge at rest (x={x} w={w})"
                );
            }
            for j in 1..row.x.len() {
                let join = row.x[j] - (row.x[j - 1] + row.w[j - 1]);
                assert!((join - g).abs() < 0.05, "resting row join {join}");
            }
        }
        assert!(
            count.iter().all(|&n| n == 1),
            "every clip rests in exactly one row"
        );
    }

    /// The settled packing is a FIXED POINT: taking a fresh grip on it and
    /// walking the ribbon at the same zoom reproduces the resting layout
    /// exactly, so a new pinch starts without a jump. (The mockup's early
    /// versions failed here — a ragged resting row made the walk pull the
    /// next chip up on the gesture's first frame.)
    #[test]
    fn ribbon_settled_layout_is_a_fixed_point() {
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let mut app = ribbon_app(vp);
        app.event(InputEvent::CursorMoved { x: 700.0, y: 420.0 });
        for _ in 0..25 {
            app.event(InputEvent::Pinch { delta: -0.02 }); // zoom out
            let _ = app.frame(0.016, vp);
        }
        if let Some(g) = &mut app.ribbon {
            g.last = Instant::now() - Duration::from_millis(400);
        }
        for _ in 0..80 {
            let _ = app.frame(0.016, vp);
            if app.ribbon_settle.is_none() && app.ribbon.is_none() {
                break;
            }
        }
        assert!(app.ribbon_pack.is_some(), "a packing settled");

        // A fresh grip anywhere must reproduce the layout it grips.
        for &(cx, cy) in &[(300.0, 200.0), (900.0, 500.0), (1270.0, 90.0)] {
            app.event(InputEvent::CursorMoved { x: cx, y: cy });
            let lay = app.layout();
            let grip = app.ribbon_grip(&lay).expect("a grip");
            let (rows, _) = app.ribbon_walk(&grip, app.zoom, false);
            assert_ribbon_invariants(&app, &rows, "fresh grip");
            for row in &rows {
                for it in &row.items {
                    let (x, y, w, _) = app.tile_rect(&lay, it.clip);
                    assert!(
                        (it.x - x).abs() < 0.05
                            && (it.w - w).abs() < 0.05
                            && (row.y + app.scroll - y).abs() < 0.05,
                        "grip at ({cx}, {cy}) moved clip {} on its first frame",
                        it.clip,
                    );
                }
            }
        }
    }

    /// Many pinches in a row, gripped all over the window and pulled both
    /// ways, never break the row rules and never leave a clip clipped,
    /// duplicated or lost at rest — and every settled layout stays a fixed
    /// point for the next grip. This is the shape of test that caught both
    /// real bugs in the prototype (a stale grip jerking the row sideways,
    /// and a row emptying into a dead band), so it runs the app the way a
    /// user actually browses rather than one clean gesture.
    #[test]
    fn ribbon_survives_a_long_run_of_pinches() {
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let mut app = ribbon_app(vp);
        let mut seed: u32 = 0x9e37_79b9;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32
        };
        for g in 0..24 {
            app.event(InputEvent::CursorMoved {
                x: 20.0 + rnd() * (vp.width - 40.0),
                y: 20.0 + rnd() * (vp.height - 40.0),
            });
            let dir = if rnd() < 0.5 { 0.02 } else { -0.02 };
            let steps = 3 + (rnd() * 20.0) as usize;
            for k in 0..steps {
                app.event(InputEvent::Pinch { delta: dir });
                let _ = app.frame(0.016, vp);
                if let Some(grip) = &app.ribbon {
                    let (rows, _) = app.ribbon_walk(grip, app.zoom, false);
                    assert_ribbon_invariants(&app, &rows, &format!("run {g} pinch {k}"));
                }
            }
            // Let go and let it settle.
            if let Some(grip) = &mut app.ribbon {
                grip.last = Instant::now() - Duration::from_millis(400);
            }
            for _ in 0..120 {
                let _ = app.frame(0.016, vp);
                if let Some(rows) = app.ribbon_rows() {
                    assert_ribbon_invariants(&app, &rows, &format!("run {g} settle"));
                } else {
                    break;
                }
            }
            assert!(app.ribbon.is_none() && app.ribbon_settle.is_none());

            // At rest: a partition of the library, nothing clipped.
            let lay = app.layout();
            let flex = lay.flex.clone().expect("flexible rows");
            let gap = app.tuning.gap;
            let mut count = vec![0u32; app.clips.len()];
            for row in &flex.rows {
                for i in row.start..row.end {
                    count[i] += 1;
                }
                for (j, (&x, &w)) in row.x.iter().zip(&row.w).enumerate() {
                    assert!(
                        x >= gap - 0.05 && x + w <= vp.width - gap + 0.05,
                        "run {g}: clip {} is clipped at rest",
                        row.start + j
                    );
                }
            }
            assert!(
                count.iter().all(|&n| n == 1),
                "run {g}: every clip rests in exactly one row"
            );

            // ...and a fresh grip on it moves nothing.
            app.event(InputEvent::CursorMoved {
                x: 20.0 + rnd() * (vp.width - 40.0),
                y: 20.0 + rnd() * (vp.height - 40.0),
            });
            let lay = app.layout();
            if let Some(grip) = app.ribbon_grip(&lay) {
                let (rows, _) = app.ribbon_walk(&grip, app.zoom, false);
                for row in &rows {
                    for it in &row.items {
                        let (x, y, w, _) = app.tile_rect(&lay, it.clip);
                        assert!(
                            (it.x - x).abs() < 0.05
                                && (it.w - w).abs() < 0.05
                                && (row.y + app.scroll - y).abs() < 0.05,
                            "run {g}: a fresh grip moved clip {}",
                            it.clip
                        );
                    }
                }
            }
        }
    }

    /// The last frame of the release morph and the first frame after it
    /// must draw the same grid. Any difference is a snap at the handoff —
    /// the class of bug that put chip 0 back against the left margin the
    /// moment the grid came to rest.
    #[test]
    fn ribbon_handoff_does_not_move_the_grid() {
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let mut app = ribbon_app(vp);
        app.event(InputEvent::CursorMoved { x: 640.0, y: 360.0 });
        for _ in 0..30 {
            app.event(InputEvent::Pinch { delta: 0.02 });
            let _ = app.frame(0.016, vp);
        }
        if let Some(g) = &mut app.ribbon {
            g.last = Instant::now() - Duration::from_millis(400);
        }
        let _ = app.frame(0.016, vp);
        // Last frame the morph drew, keeping each chip's ON-SCREEN copy
        // (a straddler's other copy is deliberately parked past the edge).
        let mut last: std::collections::HashMap<usize, (f32, f32)> = std::collections::HashMap::new();
        for _ in 0..200 {
            let Some(rows) = app.ribbon_rows() else { break };
            last.clear();
            let mut vis: std::collections::HashMap<usize, f32> = std::collections::HashMap::new();
            for row in &rows {
                for it in &row.items {
                    // How much of this copy the window actually shows; a
                    // departing copy is a sliver at the edge, so the copy
                    // that landed always wins.
                    let seen = (it.x + it.w).min(vp.width) - it.x.max(0.0);
                    if seen > 2.0 && seen > *vis.get(&it.clip).unwrap_or(&0.0) {
                        vis.insert(it.clip, seen);
                        // Viewport space: installing the packing also
                        // rebases the scroll, so content coords are not
                        // comparable across the handoff — what must not
                        // move is where the chip sits on screen.
                        last.insert(it.clip, (it.x, row.y));
                    }
                }
            }
            let _ = app.frame(0.016, vp);
        }
        assert!(!last.is_empty(), "the morph drew something");
        assert!(app.ribbon_pack.is_some(), "and installed its packing");
        let lay = app.layout();
        for (&clip, &(x, y)) in &last {
            let (tx, ty_content, _, _) = app.tile_rect(&lay, clip);
            let ty = ty_content - app.scroll;
            assert!(
                (tx - x).abs() < 1.5 && (ty - y).abs() < 1.5,
                "handoff moved clip {clip} from ({x}, {y}) to ({tx}, {ty})"
            );
        }
    }

    /// A thumb landing changes a clip's aspect and bumps `grid_rev`. The
    /// settled packing must absorb that in place — a row that no longer
    /// fits hands a chip down — and NOT collapse back to the justified
    /// packing, which would re-align every row and fling chip 0 to the
    /// left margin a moment after the grid settled. (The demo library
    /// never streams thumbs, so only an explicit arrival catches this.)
    #[test]
    fn ribbon_packing_survives_a_thumb_arrival() {
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let mut app = ribbon_app(vp);
        app.event(InputEvent::CursorMoved { x: 900.0, y: 300.0 });
        for _ in 0..30 {
            app.event(InputEvent::Pinch { delta: 0.02 });
            let _ = app.frame(0.016, vp);
        }
        if let Some(g) = &mut app.ribbon {
            g.last = Instant::now() - Duration::from_millis(400);
        }
        for _ in 0..200 {
            let _ = app.frame(0.016, vp);
            if app.ribbon_settle.is_none() {
                break;
            }
        }
        let starts = app
            .ribbon_pack
            .as_ref()
            .expect("a packing settled")
            .starts
            .clone();
        let indent_before = {
            let lay = app.layout();
            let flex = lay.flex.clone().expect("flexible rows");
            flex.rows[0].x[0]
        };

        // Thumbs land: aspects change, grid_rev bumps, layout rebuilds.
        for i in [3usize, 9, 17, 40] {
            app.clips[i].aspect = Some(2.35);
        }
        app.grid_rev = app.grid_rev.wrapping_add(1);
        app.flex_cache.borrow_mut().take();
        let _ = app.frame(0.016, vp);

        assert!(
            app.ribbon_pack.is_some(),
            "the packing survives an arrival instead of being dropped"
        );
        let lay = app.layout();
        let flex = lay.flex.clone().expect("flexible rows");
        // Still the ribbon packing (healed, not re-justified): the rows it
        // settled with are still there, and nothing is clipped.
        assert_eq!(
            flex.rows[0].start, starts[0],
            "row 0 keeps the membership the gesture left it"
        );
        let g = app.tuning.gap;
        for row in &flex.rows {
            for (j, (&x, &w)) in row.x.iter().zip(&row.w).enumerate() {
                assert!(
                    x >= g - 0.05 && x + w <= vp.width - g + 0.05,
                    "clip {} is clipped after the arrival",
                    row.start + j
                );
            }
        }
        // The symptom to catch is HORIZONTAL: a re-justify slams row 0
        // back against the left margin. Rows changing height (and so the
        // rows below shifting down) is the reflow an arrival always
        // caused and is not what the user is seeing.
        let indent_after = flex.rows[0].x[0];
        assert!(
            (indent_after - indent_before).abs() < 1.0,
            "row 0 slid from an indent of {indent_before} to {indent_after} on an arrival"
        );
    }

    /// A pinch on a BIG library must stay interactive. The release walks
    /// the whole strip to quantize it, so anything quadratic in there
    /// freezes the main thread for seconds at every gesture — which is
    /// what prepending rows one at a time into a growing Vec did (and it
    /// read as the app hanging, since an occluded window still runs the
    /// frame loop). The demo library is far too small to expose it.
    #[test]
    fn ribbon_release_stays_linear_on_a_big_library() {
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let mut app = ribbon_app(vp);
        // ~12k clips, the scale of a real scraped library.
        let now = Instant::now() - Duration::from_secs(5);
        for i in app.clips.len()..12_000 {
            app.clips.push(Clip {
                path: PathBuf::from(format!("big/clip_{i:05}.mp4")),
                readable: true,
                cloud: false,
                cached: true,
                spawned: now,
                scale: 1.0,
                emph: 0.0,
                thumb: Thumb::Failed,
                anim: Thumb::Failed,
                aspect: Some([16.0 / 9.0, 9.0 / 16.0, 1.0, 2.35, 4.0 / 3.0][i % 5]),
                created: None,
            });
        }
        app.grid_rev = app.grid_rev.wrapping_add(1);
        app.flex_cache.borrow_mut().take();
        let _ = app.frame(0.016, vp);

        // Grip well down the library so the walk has to climb thousands of
        // rows back to clip 0 — the quadratic's worst case.
        app.scroll = 40_000.0;
        app.scroll_target = app.scroll;
        app.event(InputEvent::CursorMoved { x: 640.0, y: 400.0 });
        let t0 = Instant::now();
        for _ in 0..20 {
            app.event(InputEvent::Pinch { delta: 0.02 });
            let _ = app.frame(0.016, vp);
        }
        let per_frame = t0.elapsed().as_secs_f32() / 20.0;
        if let Some(g) = &mut app.ribbon {
            g.last = Instant::now() - Duration::from_millis(400);
        }
        let t1 = Instant::now();
        let _ = app.frame(0.016, vp); // the release frame: full walk + quantize
        let release = t1.elapsed();
        let mut frames = 0;
        while app.ribbon_settle.is_some() && frames < 300 {
            let _ = app.frame(0.016, vp);
            frames += 1;
        }
        // The packing must be a clean partition with nothing clipped, at
        // the scale where a degenerate row is easy to miss by eye.
        {
            let lay = app.layout();
            let flex = lay.flex.clone().expect("flexible rows");
            let gap = app.tuning.gap;
            let mut covered = 0usize;
            let mut over = 0usize;
            for row in &flex.rows {
                assert!(row.end > row.start, "empty row");
                assert_eq!(row.start, covered, "rows must partition the library in order");
                covered = row.end;
                let right = row.x.last().unwrap() + row.w.last().unwrap();
                if right > vp.width - gap + 0.5 || row.x[0] < gap - 0.5 { over += 1; }
            }
            assert_eq!(covered, app.clips.len(), "the packing covers every clip once");
            assert_eq!(over, 0, "{over} rows overflow the window at rest");
        }
        assert!(app.ribbon_settle.is_none(), "the settle finished");
        assert!(
            app.ribbon_pack.is_some(),
            "and installed a packing for 12k clips"
        );
        // Generous bounds: the point is catching an O(n²) blowup (which
        // took SECONDS here), not policing exact timings.
        assert!(
            release < Duration::from_millis(400),
            "the release frame took {release:?} on 12k clips"
        );
        assert!(
            per_frame < 0.05,
            "gesture frames averaged {per_frame}s on 12k clips"
        );
    }

    /// A gesture must always end on its own, and the grid must come back
    /// to its plain layout. A ribbon left live keeps drawing its mid-flight
    /// form (rows carrying different edge offsets — the "brick" look) and
    /// keeps the frame loop hot, which on an unfocused window burns a core
    /// with nothing on screen. Three ways it could stick, all covered here:
    /// the release timer never firing, zero-delta pinches (macOS sends them
    /// around a gesture, and every event is a no-op at the zoom clamps)
    /// refreshing it forever, and losing focus mid-pinch.
    #[test]
    fn ribbon_gesture_always_ends() {
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        // 1. Real timing: stop pinching and let the wall clock do the work.
        let mut app = ribbon_app(vp);
        app.event(InputEvent::CursorMoved { x: 640.0, y: 360.0 });
        for _ in 0..15 {
            app.event(InputEvent::Pinch { delta: 0.02 });
            let _ = app.frame(0.016, vp);
        }
        assert!(app.ribbon.is_some(), "the pinch took a grip");
        std::thread::sleep(Duration::from_millis(220));
        for _ in 0..200 {
            let _ = app.frame(0.016, vp);
            if app.ribbon.is_none() && app.ribbon_settle.is_none() {
                break;
            }
        }
        assert!(
            app.ribbon_rows().is_none(),
            "the gesture ended and the grid is back to its layout"
        );
        {
            let lay = app.layout();
            let mut worst = 0.0f32; let mut who = 0;
            for (i, r) in app.reflow.iter().enumerate().filter(|(_, r)| r.init) {
                let (x, y, w, h) = app.tile_rect(&lay, i);
                for (c, t) in r.rect.iter().zip([x, y, w, h]) {
                    if (t - c).abs() > worst { worst = (t - c).abs(); who = i; }
                }
            }
            eprintln!("DBG motion={} zoom_d={} scroll_d={} reflow_init={} worst={worst} clip={who} wraps={}",
                app.motion, (app.zoom_target-app.zoom).abs(), (app.scroll_target-app.scroll).abs(),
                app.reflow.iter().filter(|r| r.init).count(),
                app.reflow.iter().filter(|r| r.wrap.is_some()).count());
        }
        // The zoom changed, so the selection's scale spring legitimately
        // re-targets; what matters is that the loop returns to idle. Wake
        // deadlines are wall-clock, so a tight frame loop has to let real
        // time pass or it measures its own compression.
        let mut settled = false;
        for _ in 0..140 {
            std::thread::sleep(Duration::from_millis(16));
            let f = app.frame(0.016, vp);
            if !f.animating {
                settled = true;
                break;
            }
        }
        assert!(settled, "the loop went back to idle after the gesture");

        // 2. Zero-delta pinches must not hold the gesture open. (Same at
        // the zoom clamps, where every event computes the same zoom.)
        let mut app = ribbon_app(vp);
        app.event(InputEvent::CursorMoved { x: 640.0, y: 360.0 });
        app.event(InputEvent::Pinch { delta: 0.05 });
        let _ = app.frame(0.016, vp);
        let held = app.ribbon.as_ref().map(|g| g.last).expect("a grip");
        std::thread::sleep(Duration::from_millis(20));
        app.event(InputEvent::Pinch { delta: 0.0 });
        assert_eq!(
            app.ribbon.as_ref().map(|g| g.last),
            Some(held),
            "a zero-delta pinch does not refresh the release timer"
        );

        // 3. Losing focus mid-pinch ends it immediately.
        let mut app = ribbon_app(vp);
        app.event(InputEvent::CursorMoved { x: 640.0, y: 360.0 });
        for _ in 0..10 {
            app.event(InputEvent::Pinch { delta: 0.02 });
            let _ = app.frame(0.016, vp);
        }
        assert!(app.ribbon.is_some(), "pinching");
        app.event(InputEvent::Focus { focused: false });
        assert!(app.ribbon.is_none(), "defocus ended the gesture");
        for _ in 0..200 {
            let _ = app.frame(0.016, vp);
            if app.ribbon_settle.is_none() {
                break;
            }
        }
        assert!(
            app.ribbon_rows().is_none(),
            "and it settled instead of holding the ribbon layout while away"
        );
    }

    /// A pinch pins the render loop at full rate for as long as the
    /// fingers are down, so the media queue's per-frame upload budget has
    /// to be squeezed while one is in flight — at the full budget that is
    /// thousands of thumb uploads a second staged mid-gesture, which is
    /// what locked the app up whenever the background sweep had results to
    /// deliver (a small library, with nothing in flight, never showed it).
    #[test]
    fn a_pinch_squeezes_the_media_upload_budget() {
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let mut app = ribbon_app(vp);
        assert_eq!(
            app.media_upload_budget(false),
            MEDIA_UPLOAD_BUDGET,
            "a settled grid drains the queue at full rate"
        );
        assert_eq!(
            app.media_upload_budget(true),
            MEDIA_UPLOAD_BUDGET_LIVE,
            "a playing stream already squeezes it"
        );
        app.event(InputEvent::CursorMoved { x: 640.0, y: 360.0 });
        app.event(InputEvent::Pinch { delta: 0.02 });
        assert!(app.ribbon.is_some(), "pinching");
        assert_eq!(
            app.media_upload_budget(false),
            MEDIA_UPLOAD_BUDGET_GESTURE,
            "a gesture squeezes it hardest"
        );
        // ...and through the settle, which is still the gesture's motion.
        if let Some(g) = &mut app.ribbon {
            g.last = Instant::now() - Duration::from_millis(400);
        }
        let _ = app.frame(0.016, vp);
        assert!(app.ribbon_settle.is_some(), "settling");
        assert_eq!(app.media_upload_budget(false), MEDIA_UPLOAD_BUDGET_GESTURE);
        // Back to full once the grid is at rest.
        for _ in 0..200 {
            let _ = app.frame(0.016, vp);
            if app.ribbon_settle.is_none() {
                break;
            }
        }
        assert_eq!(
            app.media_upload_budget(false),
            MEDIA_UPLOAD_BUDGET,
            "and the sweep gets its throughput back the moment it settles"
        );
    }

    /// A keyboard zoom has no grip to hold the grid by, so it drops the
    /// ribbon packing and returns to the justified layout (and its wrap
    /// reflow) — the way back to a clean grid after any amount of pinching.
    #[test]
    fn keyboard_zoom_returns_to_the_justified_grid() {
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        let mut app = ribbon_app(vp);
        app.event(InputEvent::CursorMoved { x: 640.0, y: 360.0 });
        for _ in 0..20 {
            app.event(InputEvent::Pinch { delta: 0.02 });
            let _ = app.frame(0.016, vp);
        }
        if let Some(g) = &mut app.ribbon {
            g.last = Instant::now() - Duration::from_millis(400);
        }
        for _ in 0..80 {
            let _ = app.frame(0.016, vp);
            if app.ribbon_settle.is_none() {
                break;
            }
        }
        assert!(app.ribbon_pack.is_some(), "pinching left a packing");
        app.set_zoom(1.4);
        assert!(app.ribbon_pack.is_none(), "a keyboard zoom drops it");
        let _ = app.frame(0.016, vp);
        let lay = app.layout();
        let flex = lay.flex.clone().expect("flexible rows");
        // Justified again: every full row starts hard against the margin.
        for row in flex.rows.iter().take(flex.rows.len().saturating_sub(1)) {
            assert!(
                (row.x[0] - app.tuning.gap).abs() < 0.05,
                "the justified packing left-aligns its rows"
            );
        }
    }

    /// A zoom reflow in the flexible grid wraps the tiles that cross a row
    /// boundary — each leaves a `WrapEvent` (forward on zoom-in) so it slides
    /// off one edge and re-enters on the next row, adding an exit copy to the
    /// frame — instead of firing the column-count crossfade. Wraps clear after
    /// `zoom_wrap_ms` and every displayed rect settles onto its layout slot.
    #[test]
    fn flexible_zoom_wraps_tiles_across_row_boundaries() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::Minimal), // ui() true, so the wrap path runs
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        let vp = Viewport {
            width: 1280.0,
            height: 800.0,
        };
        // Demo tiles fade in on a real-clock stagger into the future; backdate
        // them so they're fully spawned (this tight loop advances no real time,
        // and Minimal keeps fades real — unlike the None most tests use).
        let past = Instant::now() - Duration::from_secs(5);
        for c in &mut app.clips {
            c.spawned = past;
        }
        // First frame seeds the reflow table at zoom 1.0.
        let _ = app.frame(0.016, vp);
        assert!(
            app.reflow_active(&app.layout()),
            "wrap path is on for flexible + minimal animation"
        );
        assert!(
            app.reflow.iter().any(|r| r.init),
            "the first frame seeded the reflow table"
        );
        let rows0: Vec<u32> = {
            let lay = app.layout();
            (0..app.clips.len())
                .map(|i| app.row_of(&lay, i) as u32)
                .collect()
        };

        // A zoom keypress: the target moves, and in wrap mode the layout zoom
        // snaps to it in ONE frame (one repack — springing through
        // intermediate packings restarted wraps mid-flight, the ghost-fade
        // bug). The per-clip tweens carry the visible motion instead.
        app.set_zoom(1.7);
        let f1 = app.frame(0.016, vp);
        assert!(
            (app.zoom - 1.7).abs() < 1e-6,
            "wrap mode snaps the layout zoom to its target in one frame"
        );

        let wrapped: Vec<usize> = app
            .reflow
            .iter()
            .enumerate()
            .filter(|(_, r)| r.wrap.is_some())
            .map(|(i, _)| i)
            .collect();
        assert!(
            !wrapped.is_empty(),
            "a zoom-in reflow wraps boundary-crossing tiles"
        );
        for &i in &wrapped {
            let w = app.reflow[i].wrap.unwrap();
            assert!(w.forward, "clip {i} wraps forward on zoom-in (exit right)");
            assert_ne!(
                w.from[0], app.reflow[i].rect[0],
                "clip {i} left a slot different from where it lands"
            );
        }
        assert!(
            app.transition.is_none(),
            "the flexible grid wraps its zoom reflow instead of crossfading"
        );

        // Let the wrap window elapse; capture a settled frame at the SAME zoom.
        for _ in 0..40 {
            let _ = app.frame(0.016, vp);
        }
        let settled = app.frame(0.016, vp);
        assert!(
            app.reflow.iter().all(|r| r.wrap.is_none()),
            "every wrap clears after zoom_wrap_ms"
        );
        assert!(
            f1.tiles.len() > settled.tiles.len(),
            "exit copies added tiles during the wrap ({} vs settled {})",
            f1.tiles.len(),
            settled.tiles.len()
        );

        let lay = app.layout();
        let (first, last) = app.visible_rows(&lay, 0);
        for row in first..=last {
            for i in app.row_range(&lay, row) {
                let (x, y, w, h) = app.tile_rect(&lay, i);
                let r = app.reflow[i].rect;
                assert!(
                    (r[0] - x).abs() < 0.6
                        && (r[1] - y).abs() < 0.6
                        && (r[2] - w).abs() < 0.6
                        && (r[3] - h).abs() < 0.6,
                    "clip {i} reflow rect settles onto its layout slot"
                );
            }
        }
        let rows1: Vec<u32> = (0..app.clips.len())
            .map(|i| app.row_of(&lay, i) as u32)
            .collect();
        assert_ne!(rows0, rows1, "the zoom-in actually changed the row packing");

        // Regression: a real library bumps grid_rev constantly as thumbs and
        // aspects land. That must neither reset the wrap table mid-zoom nor
        // kill in-flight wraps (both made the whole grid snap). Zoom while
        // thrashing grid_rev every frame: wraps fire on the repack frame and
        // keep running through later arrival frames.
        app.set_zoom(app.zoom_target * 1.3);
        let mut saw_wrap = false;
        let mut survived = false;
        for f in 0..8 {
            app.grid_rev = app.grid_rev.wrapping_add(1); // a thumb "arrives"
            let _ = app.frame(0.016, vp);
            if app.reflow.iter().any(|r| r.wrap.is_some()) {
                saw_wrap = true;
                if f >= 2 {
                    survived = true; // still wrapping frames after its start
                }
            }
        }
        assert!(
            saw_wrap && survived,
            "zoom wraps fire and outlive grid_rev bumps from arrivals \
             (saw {saw_wrap}, survived {survived})"
        );

        // zoom_wrap off: the path disengages and the table is dropped (the
        // fixed-grid/shuffle crossfade takes over instead).
        app.tuning.zoom_wrap = false;
        let _ = app.frame(0.016, vp);
        assert!(
            app.reflow.is_empty(),
            "turning zoom_wrap off clears the reflow table"
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
        app.event(InputEvent::MouseDown {
            x: cx,
            y: cy,
            mods: Mods::default(),
        });
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

    /// The context-sensitive `move_left`/`move_right` (hjkl/arrows): in
    /// fullview they step the playing stream between chapters — next
    /// wraps past the last, back restarts a chapter when well into it —
    /// while before the probe answers they fall back to a fraction skip,
    /// and outside fullview they move the selection, never the stream.
    /// Needs ffmpeg; skipped quietly when missing.
    #[test]
    fn fullview_arrows_step_chapters() {
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
        let dir = std::env::temp_dir().join("sb_app_chapter_step_test");
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
            inputs: vec![clip.clone()],
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

        // Tab enters fullview through the keymap; no probe answer yet,
        // so Right falls back to a plain fraction skip within the clip.
        app.key(Key::Tab);
        assert!(app.fullview, "tab enters fullview");
        let before = app.live_sel.as_ref().unwrap().position();
        app.key(Key::Right);
        let p = app.live_sel.as_ref().unwrap().position();
        let frac = app.tuning.skip_fraction as f64;
        assert!(
            (p - before - frac * 8.0).abs() < 0.5,
            "no plan yet: a fraction skip, got {before} -> {p}"
        );

        // Fullview's prewarm fires the probe; wait for its answer.
        assert!(
            pump_until(&mut app, |a| a
                .chapter_probe
                .get(&clip)
                .is_some_and(|p| p.is_some())),
            "chapter probe never answered"
        );
        app.live_sel.as_ref().unwrap().player.seek(0.5, true);

        // Right steps chapters (starts at 0/3/6), wrapping past the last.
        app.key(Key::Right);
        let p = app.live_sel.as_ref().unwrap().position();
        assert!((2.9..=3.1).contains(&p), "right -> chapter 2, got {p}");
        // Stepping a chapter also peeks the bar open (a transient reveal,
        // distinct from a deliberate `g` open).
        assert!(
            app.chapters
                .as_ref()
                .is_some_and(|b| b.open && b.peek_until.is_some()),
            "a fullview chapter step peeks the chapter bar open"
        );
        app.key(Key::Right);
        let p = app.live_sel.as_ref().unwrap().position();
        assert!((5.9..=6.1).contains(&p), "right -> chapter 3, got {p}");
        app.key(Key::Right);
        let p = app.live_sel.as_ref().unwrap().position();
        assert!(p < 0.1, "right past the last chapter wraps, got {p}");

        // At a chapter's opening, Left steps back one; well inside a
        // chapter (past CHAPTER_RESTART_S) it restarts that chapter.
        app.live_sel.as_ref().unwrap().player.seek(6.0, true);
        app.key(Key::Left);
        let p = app.live_sel.as_ref().unwrap().position();
        assert!((2.9..=3.1).contains(&p), "left at a start steps back: {p}");
        app.live_sel.as_ref().unwrap().player.seek(5.5, true);
        app.key(Key::Left);
        let p = app.live_sel.as_ref().unwrap().position();
        assert!(
            (2.9..=3.1).contains(&p),
            "left mid-chapter restarts it, got {p}"
        );

        // The bar is only peeked (never a deliberate g-open), so Esc does
        // NOT stop at it — it exits fullview directly and the peek drops.
        app.key(Key::Escape);
        assert!(!app.fullview, "esc leaves fullview (a peek doesn't intercept)");
        assert!(app.chapters.is_none(), "the peek drops with fullview");
        let before = app.live_sel.as_ref().unwrap().position();
        app.key(Key::Right);
        assert_eq!(app.selected, 0, "one clip: selection clamps in place");
        let p = app.live_sel.as_ref().unwrap().position();
        assert!(
            (p - before).abs() < 0.5,
            "grid movement never seeks the stream: {before} -> {p}"
        );
    }

    /// The keyframe-seek fix: a chapter jump lands just *before* its start
    /// (nearest keyframe), so the raw position-derived index reports the
    /// PREVIOUS chapter. `resolve_chapter` lets the navigation intent win
    /// through that undershoot lead-in, then releases it once playback
    /// carries into a later chapter — without it the highlight janks back
    /// and forward stepping is stuck (the reported bug).
    #[test]
    fn resolve_chapter_pins_nav_through_keyframe_undershoot() {
        // No intent: pure position-derived index passes through.
        assert_eq!(resolve_chapter(0, None), 0);
        assert_eq!(resolve_chapter(2, None), 2);
        // Stepped to chapter 2, landed in its keyframe-undershoot tail
        // (position still reads chapter 1): the intent wins, so the
        // highlight is chapter 2 and the next step advances from 2.
        assert_eq!(resolve_chapter(1, Some(2)), 2);
        // Landed a touch INTO chapter 2 (keyframe on/after the boundary):
        // still chapter 2.
        assert_eq!(resolve_chapter(2, Some(2)), 2);
        // Playback carried past into chapter 3: the raw index takes over,
        // stale intent released.
        assert_eq!(resolve_chapter(3, Some(2)), 3);
        // Seeked/looped well before the intent's lead-in: raw index wins.
        assert_eq!(resolve_chapter(0, Some(2)), 0);
    }

    /// End-to-end guard for the reported regression: with keyframe chapter
    /// seeks, stepping to a chapter start lands on the nearest keyframe
    /// *before* it, so after the frame settles the decoder sits in the
    /// previous chapter. A second forward step must still ADVANCE (not
    /// restep to the same place) and the highlight must track the chapter
    /// stepped to. Chapters are deliberately off the 1s keyframe grid
    /// (`-g 30`) so the undershoot is real. Needs ffmpeg; skipped quietly.
    #[test]
    fn chapter_steps_advance_after_keyframe_undershoot_settles() {
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
        let dir = std::env::temp_dir().join("sb_app_chapter_undershoot_test");
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("plain_g30.mp4");
        let clip = dir.join("chaptered_g30.mp4");
        if !clip.exists() {
            let ok = std::process::Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=8:size=320x180:rate=30")
                .args([
                    "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "30",
                ])
                .arg(&plain)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to generate test clip");
            // Chapter starts at 0 / 2.7 / 5.3s — NOT on the 1s keyframe
            // grid, so a keyframe seek to 2.7 lands at 2.0 (chapter 0's
            // tail) and to 5.3 lands at 5.0 (chapter 1).
            let metafile = dir.join("chapters_g30.txt");
            std::fs::write(
                &metafile,
                ";FFMETADATA1\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=2700\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=2700\nEND=5300\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=5300\nEND=8000\n",
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
            inputs: vec![clip.clone()],
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
        app.key(Key::Tab);
        assert!(app.fullview, "tab enters fullview");
        assert!(
            pump_until(&mut app, |a| a
                .chapter_probe
                .get(&clip)
                .is_some_and(|p| p.is_some())),
            "chapter probe never answered"
        );

        // Step to chapter 2 (start 2.7s) and let the keyframe landing
        // SETTLE below the boundary — this is what made the old math stuck.
        app.key(Key::Right);
        assert!(
            pump_until(&mut app, |a| a
                .live_sel
                .as_ref()
                .is_some_and(|l| l.position() < 2.6 && l.position() > 1.0)),
            "keyframe seek never settled in chapter 0's tail (undershoot)"
        );
        let bar = app.chapters.as_ref().expect("bar peeked open").clone();
        assert_eq!(
            app.current_chapter(&bar),
            Some(1),
            "highlight tracks the chapter stepped to, not the undershot position"
        );

        // Second forward step: it must ADVANCE to chapter 3 (start 5.3s),
        // not restep chapter 2. Read the in-flight target immediately.
        app.key(Key::Right);
        let p = app.live_sel.as_ref().unwrap().position();
        assert!(
            (5.2..=5.4).contains(&p),
            "second step must advance to chapter 3, got {p} (regressed: stuck at chapter 2)"
        );
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

    /// Attention mode (`interaction = "attention"`, DESIGN.md §15 spike):
    /// a single click on ANY grid tile selects it and opens quickview on
    /// release — while a matured drag-out still suppresses the open, and
    /// classic mode keeps its select-first behavior.
    #[test]
    fn attention_click_opens_quickview_on_any_tile() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        app.tuning.interaction = Interaction::Attention;
        let lay = app.layout();
        let (tx, ty, tw, th) = app.tile_rect(&lay, 1);
        let (cx, cy) = (tx + tw * 0.5, ty + th * 0.5);
        assert_eq!(
            app.tile_at(&lay, cx, cy),
            Some(1),
            "test point misses tile 1"
        );

        // Click an UNSELECTED tile: selects on press, quickviews on release.
        app.event(InputEvent::MouseDown {
            x: cx,
            y: cy,
            mods: Mods::default(),
        });
        assert_eq!(app.selected, 1, "click selects the tile");
        assert!(!app.quickview, "quickview waits for the release");
        app.event(InputEvent::MouseUp { x: cx, y: cy });
        assert!(app.quickview, "a single click on any tile quickviews");

        // A drag from an unselected tile must NOT open the modal.
        app.quickview = false;
        let (tx0, ty0, tw0, th0) = app.tile_rect(&lay, 0);
        let (dx, dy) = (tx0 + tw0 * 0.5, ty0 + th0 * 0.5);
        app.event(InputEvent::MouseDown {
            x: dx,
            y: dy,
            mods: Mods::default(),
        });
        let far = app.tuning.drag_threshold + 10.0;
        app.event(InputEvent::CursorMoved { x: dx + far, y: dy });
        app.event(InputEvent::MouseUp { x: dx + far, y: dy });
        assert!(!app.quickview, "a matured drag must not open quickview");
        assert!(
            app.cmds
                .iter()
                .any(|c| matches!(c, WindowCommand::BeginDrag { .. })),
            "the drag still matured into a drag-out"
        );

        // Classic mode: a click on an unselected tile only selects.
        app.tuning.interaction = Interaction::Classic;
        app.event(InputEvent::MouseDown {
            x: cx,
            y: cy,
            mods: Mods::default(),
        });
        app.event(InputEvent::MouseUp { x: cx, y: cy });
        assert!(!app.quickview, "classic: first click only selects");
    }

    /// Attention mode: cmd-click toggles a border-only mark, shift-click
    /// marks the range from the selection — neither opens quickview,
    /// moves the selection, or feeds the playback lanes.
    #[test]
    fn attention_multi_select_marks_without_playing() {
        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::None),
            demo: true,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        app.tuning.interaction = Interaction::Attention;
        let lay = app.layout();
        let center = |app: &Switchblade, i: usize| {
            let (tx, ty, tw, th) = app.tile_rect(&lay, i);
            (tx + tw * 0.5, ty + th * 0.5)
        };
        let cmd = Mods {
            cmd: true,
            shift: false,
        };

        let (x, y) = center(&app, 2);
        app.event(InputEvent::MouseDown { x, y, mods: cmd });
        app.event(InputEvent::MouseUp { x, y });
        assert!(app.marked.contains(&2), "cmd-click marks the tile");
        assert!(!app.quickview, "marking never opens quickview");
        assert_eq!(app.selected, 0, "marking never moves the selection");
        assert_eq!(
            app.attention_target(),
            Some(0),
            "a mark never becomes the attention lane's target"
        );

        // Toggle off.
        app.event(InputEvent::MouseDown { x, y, mods: cmd });
        assert!(!app.marked.contains(&2), "cmd-click again unmarks");

        // Shift-click ranges from the selection.
        let (x3, y3) = center(&app, 3);
        app.event(InputEvent::MouseDown {
            x: x3,
            y: y3,
            mods: Mods {
                cmd: false,
                shift: true,
            },
        });
        assert_eq!(
            app.marked,
            [0usize, 1, 2, 3].into_iter().collect(),
            "shift-click marks the whole range from the selection"
        );
        assert_eq!(app.selected, 0, "range-marking holds the selection");

        // Classic mode ignores the modifiers entirely: a plain select.
        app.marked.clear();
        app.tuning.interaction = Interaction::Classic;
        app.event(InputEvent::MouseDown { x, y, mods: cmd });
        assert!(app.marked.is_empty(), "classic: no marks");
        assert_eq!(app.selected, 2, "classic: the click just selects");
    }

    /// Attention mode: the ONE hires lane follows the pointer — hovering
    /// an unselected tile (after the attention settle delay) retargets
    /// the selected-stream lane to that clip without moving the
    /// selection, and the grid's tile-size hover lane never spawns. A
    /// keyboard move hands the lane back to the selection. Needs ffmpeg;
    /// skipped quietly when it's not on PATH.
    #[test]
    fn attention_lane_follows_hover_then_keyboard() {
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
        let dir = std::env::temp_dir().join("sb_app_attention_test");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.mp4", "b.mp4"] {
            let clip = dir.join(name);
            if clip.exists() {
                continue;
            }
            let ok = std::process::Command::new("ffmpeg")
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
            assert!(ok, "failed to generate test clip {name}");
        }

        let mut app = Switchblade::with_options(Options {
            animation: Some(AnimLevel::Normal),
            inputs: vec![dir.clone()],
            demo: false,
            no_config: true, // hermetic: the host config must not steer tests
            ..Options::default()
        });
        app.tuning.interaction = Interaction::Attention;
        assert!(
            pump_until(&mut app, |a| a.clips.len() == 2),
            "ingest never delivered both clips"
        );
        // Keyboard owns attention at start: the lane plays the selection.
        assert!(
            pump_until(&mut app, |a| a
                .live_sel
                .as_ref()
                .is_some_and(|l| l.clip == 0 && l.first_frame.is_some())),
            "attention lane never started on the selection"
        );

        // Hover the OTHER tile: after the attention settle the lane
        // retargets to it — the selection stays put.
        let lay = app.layout();
        let (tx, ty, tw, th) = app.tile_rect(&lay, 1);
        app.event(InputEvent::CursorMoved {
            x: tx + tw * 0.5,
            y: ty + th * 0.5,
        });
        assert!(
            pump_until(&mut app, |a| a
                .live_sel
                .as_ref()
                .is_some_and(|l| l.clip == 1 && l.first_frame.is_some())),
            "attention lane never followed the hover"
        );
        assert_eq!(
            app.selected, 0,
            "hover-attention must not move the selection"
        );
        assert!(
            app.live_hover.is_none(),
            "attention mode never spawns the tile-size hover lane"
        );

        // A keyboard move reclaims the lane for the selection.
        app.move_selection(1, 0);
        assert_eq!(app.selected, 1);
        assert!(!app.mouse_attention, "keyboard move reclaims attention");
        assert_eq!(app.attention_target(), Some(1));
    }
}
