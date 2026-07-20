//! Tier-A headless benchmark runner (benchmarks/TASKS.md Phase 3;
//! benchmarks/design/phase-0-contracts.md).
//!
//! Drives the real `Switchblade` through its `App` interface with real
//! decoders, real ffmpeg workers, and a real (temp-`HOME`) cache — no GPU,
//! no window. A scenario is setup + a sequence of steps + a prose intent;
//! the runner only measures. Interpretation is retrospective and agentic
//! (the summary + events feed a reader), so nothing here judges a number
//! "good" or "bad" — only mechanical validity (did the run complete and
//! meet its required conditions).
//!
//! **Pacing.** The real window paces the *animating* loop on the blocking
//! vsync present (~refresh Hz), and the *idle* loop on
//! `sb_window::schedule::next_frame` (the 10Hz housekeeping tick capped by
//! the live-video `redraw_at` deadline). A headless loop has no vsync, so
//! it would free-run the animating path at the `MIN_FRAME` floor (~250fps)
//! and inflate per-frame `drain_media` throughput — exactly the fidelity
//! trap Phase 0 called out. So the runner stands in for vsync at a
//! configured refresh on the animating path and defers to the shared
//! scheduler on the idle path.

use crate::{AnimLevel, Options, Switchblade};
use sb_window::{schedule, App, InputEvent, Key, Mods, Viewport};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Scenario schema (TOML)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub intent: String,
    pub setup: Setup,
    #[serde(default)]
    pub step: Vec<Step>,
    #[serde(default)]
    pub validity: Validity,
}

#[derive(Debug, Deserialize)]
pub struct Setup {
    /// Fixture basenames resolved under `benchmarks/fixtures/` (or the dir
    /// named by `$SB_BENCH_FIXTURES`). Alternative to `inputs`.
    #[serde(default)]
    pub fixtures: Vec<String>,
    /// Explicit paths (absolute, or relative to the cwd). Wins over
    /// `fixtures` when both are set.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// "none" | "minimal" | "normal" (default: live video on).
    #[serde(default = "default_animation")]
    pub animation: String,
    /// Logical viewport [width, height].
    #[serde(default = "default_viewport")]
    pub viewport: [f32; 2],
    #[serde(default)]
    pub demo: bool,
    /// The vsync stand-in for the animating loop (see the module docs).
    #[serde(default = "default_refresh")]
    pub refresh_hz: f64,
    /// Hard ceiling on total wall time; a stuck scenario fails validity
    /// rather than hanging.
    #[serde(default = "default_max_wall")]
    pub max_wall_s: f64,
}

fn default_animation() -> String {
    "normal".into()
}
fn default_viewport() -> [f32; 2] {
    [1280.0, 800.0]
}
fn default_refresh() -> f64 {
    60.0
}
fn default_max_wall() -> f64 {
    120.0
}

/// One scripted step, discriminated by `action`:
/// - `wait` (`secs`) — drive frames for that long
/// - `wait_until` (`cond`, `n`, `timeout`) — until a condition or timeout
/// - `key` (`key`) — inject a key ("space","escape","h".."char")
/// - `hover` (`target`) — move the cursor to a tile's center
/// - `click` (`target`) — mouse down+up on a tile
/// - `scroll` (`dy`) — wheel/trackpad scroll
#[derive(Debug, Deserialize)]
pub struct Step {
    pub action: String,
    #[serde(default)]
    pub secs: f64,
    #[serde(default)]
    pub cond: String,
    #[serde(default)]
    pub n: usize,
    #[serde(default)]
    pub timeout: f64,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub dy: f32,
}

#[derive(Debug, Default, Deserialize)]
pub struct Validity {
    /// Conditions that MUST have been met for the run to count (mechanical,
    /// never a performance judgment).
    #[serde(default)]
    pub require: Vec<String>,
}

// ---------------------------------------------------------------------------
// Report schema (JSON)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct Summary {
    pub scenario: String,
    pub intent: String,
    pub valid: bool,
    pub invalid_reasons: Vec<String>,
    pub wall_s: f64,
    pub frames: u64,
    pub counters: sb_media::CounterSnapshot,
    pub latencies: Vec<LatencyStat>,
    /// [t_seconds, thumbs_cached] samples (compacted: one point per change).
    pub thumbs_curve: Vec<[f64; 2]>,
    pub tick_ms: TickStat,
    pub events: usize,
    pub events_dropped: u64,
    pub conditions: Vec<CondResult>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LatencyStat {
    pub lane: String,
    pub metric: String,
    pub count: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TickStat {
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CondResult {
    pub cond: String,
    pub met: bool,
    pub at_s: Option<f64>,
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Run a scenario file end to end, writing `summary.json` and
/// `events.jsonl` under `out_dir`, and return the summary. The caller is
/// responsible for hermetic isolation (a fresh temp `HOME` per run, set
/// BEFORE this is called — the cache root is a process-global `OnceLock`).
pub fn run(scenario_path: &Path, out_dir: &Path) -> Result<Summary, String> {
    let text = std::fs::read_to_string(scenario_path)
        .map_err(|e| format!("read {}: {e}", scenario_path.display()))?;
    let scenario: Scenario = toml::from_str(&text).map_err(|e| format!("parse scenario: {e}"))?;
    std::fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {}: {e}", out_dir.display()))?;

    let inputs = resolve_inputs(&scenario.setup)?;
    let opts = Options {
        animation: Some(AnimLevel::parse(&scenario.setup.animation).unwrap_or(AnimLevel::Normal)),
        inputs,
        demo: scenario.setup.demo,
        no_config: true, // hermetic: a stray config must never steer a bench
        ..Options::default()
    };
    let viewport = Viewport {
        width: scenario.setup.viewport[0],
        height: scenario.setup.viewport[1],
    };

    let mut rig = Rig::new(Switchblade::with_options(opts), viewport, &scenario.setup);
    rig.probe.record_events();
    rig.tick(); // boot one frame so layout/ingest exist

    let mut invalid = Vec::new();
    for step in &scenario.step {
        if let Err(e) = rig.exec(step) {
            invalid.push(e);
            break;
        }
    }

    let wall_s = rig.anchor.elapsed().as_secs_f64();
    let (events, dropped) = rig.probe.drain(rig.anchor);
    write_events_jsonl(out_dir, &events)?;

    // Mechanical validity: every step ran (no early break) + required
    // conditions all met. NEVER a performance verdict.
    let mut conditions = Vec::new();
    for req in &scenario.validity.require {
        let hit = rig.conds.iter().find(|(c, _)| c == req);
        let met = hit.is_some();
        if !met {
            invalid.push(format!("required condition never met: {req}"));
        }
        conditions.push(CondResult {
            cond: req.clone(),
            met,
            at_s: hit.map(|(_, t)| *t),
        });
    }

    let summary = Summary {
        scenario: scenario.name,
        intent: scenario.intent,
        valid: invalid.is_empty(),
        invalid_reasons: invalid,
        wall_s,
        frames: rig.probe.snapshot().frames,
        counters: rig.probe.snapshot(),
        latencies: compute_latencies(&events),
        thumbs_curve: rig.thumbs_curve,
        tick_ms: pctl_stat(&mut rig.tick_ms.clone()),
        events: events.len(),
        events_dropped: dropped,
        conditions,
    };

    let json = sb_media::serde_json::to_string_pretty(&summary)
        .map_err(|e| format!("serialize summary: {e}"))?;
    std::fs::write(out_dir.join("summary.json"), json)
        .map_err(|e| format!("write summary.json: {e}"))?;
    Ok(summary)
}

/// Per-render loop state.
struct Rig {
    app: Switchblade,
    viewport: Viewport,
    probe: std::sync::Arc<sb_media::Probe>,
    anchor: Instant,
    last_frame: Instant,
    refresh: Duration,
    max_wall: Duration,
    animating: bool,
    redraw_at: Option<Instant>,
    /// [t, thumbs_cached] compacted to changes.
    thumbs_curve: Vec<[f64; 2]>,
    last_thumbs: u64,
    /// Per-frame `frame()` wall cost in ms.
    tick_ms: Vec<f64>,
    /// First time each named condition became true (name, t_seconds).
    conds: Vec<(String, f64)>,
}

impl Rig {
    fn new(app: Switchblade, viewport: Viewport, setup: &Setup) -> Self {
        let probe = app.probe();
        let now = Instant::now();
        Rig {
            app,
            viewport,
            probe,
            anchor: now,
            last_frame: now,
            refresh: Duration::from_secs_f64(1.0 / setup.refresh_hz.max(1.0)),
            max_wall: Duration::from_secs_f64(setup.max_wall_s.max(1.0)),
            animating: true,
            redraw_at: None,
            thumbs_curve: Vec::new(),
            last_thumbs: u64::MAX,
            tick_ms: Vec::new(),
            conds: Vec::new(),
        }
    }

    /// Render exactly one frame: real dt (clamped like the window), then
    /// sample counters and the tick cost.
    fn tick(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.05);
        self.last_frame = now;
        let t0 = Instant::now();
        let frame = self.app.frame(dt, self.viewport);
        self.tick_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        self.animating = frame.animating;
        self.redraw_at = frame.redraw_at;
        let _ = self.app.commands(); // drain window commands (title, etc.)
        // Compact thumbs-cached curve: a point only when it changes.
        let tc = self.probe.snapshot().thumbs_cached;
        if tc != self.last_thumbs {
            self.last_thumbs = tc;
            self.thumbs_curve
                .push([self.anchor.elapsed().as_secs_f64(), tc as f64]);
        }
    }

    /// Sleep to the next frame boundary (vsync stand-in while animating,
    /// the shared idle scheduler otherwise), then render it.
    fn step_frame(&mut self) {
        let now = Instant::now();
        let target = if self.animating {
            // Real window paces this path on the vsync present; stand in
            // for it at the configured refresh so headless doesn't
            // free-run and inflate per-frame work.
            self.last_frame + self.refresh
        } else {
            match schedule::next_frame(false, false, self.redraw_at, self.last_frame, now) {
                schedule::NextFrame::Now { .. } => now,
                schedule::NextFrame::At(t) => t,
            }
        };
        if target > now {
            std::thread::sleep((target - now).min(Duration::from_millis(100)));
        }
        self.tick();
    }

    fn exceeded_budget(&self) -> bool {
        self.anchor.elapsed() > self.max_wall
    }

    fn exec(&mut self, step: &Step) -> Result<(), String> {
        match step.action.as_str() {
            "wait" => {
                let until = Instant::now() + Duration::from_secs_f64(step.secs);
                while Instant::now() < until {
                    if self.exceeded_budget() {
                        return Err("max_wall exceeded during wait".into());
                    }
                    self.step_frame();
                }
                Ok(())
            }
            "wait_until" => {
                let timeout = if step.timeout > 0.0 {
                    step.timeout
                } else {
                    30.0
                };
                let deadline = Instant::now() + Duration::from_secs_f64(timeout);
                loop {
                    if self.cond_met(&step.cond, step.n) {
                        let t = self.anchor.elapsed().as_secs_f64();
                        self.record_cond(&step.cond, t);
                        return Ok(());
                    }
                    if Instant::now() >= deadline || self.exceeded_budget() {
                        return Err(format!(
                            "wait_until '{}' timed out after {timeout:.1}s",
                            step.cond
                        ));
                    }
                    self.step_frame();
                }
            }
            "key" => {
                let key = parse_key(&step.key)
                    .ok_or_else(|| format!("unknown key: {:?}", step.key))?;
                self.app.event(InputEvent::Key { key, repeat: false });
                self.tick();
                Ok(())
            }
            "hover" => {
                let (x, y) = self.target_center(&step.target)?;
                self.app.event(InputEvent::CursorMoved { x, y });
                self.tick();
                Ok(())
            }
            "click" => {
                let (x, y) = self.target_center(&step.target)?;
                self.app.event(InputEvent::MouseDown {
                    x,
                    y,
                    mods: Mods::default(),
                });
                self.tick();
                self.app.event(InputEvent::MouseUp { x, y });
                self.tick();
                Ok(())
            }
            "scroll" => {
                self.app.event(InputEvent::Scroll {
                    dx: 0.0,
                    dy: step.dy,
                });
                self.tick();
                Ok(())
            }
            other => Err(format!("unknown action: {other}")),
        }
    }

    fn record_cond(&mut self, cond: &str, t: f64) {
        if !self.conds.iter().any(|(c, _)| c == cond) {
            self.conds.push((cond.to_string(), t));
        }
    }

    /// Evaluate a named readiness condition against live app state
    /// (phase-0-contracts §0.4).
    fn cond_met(&self, cond: &str, n: usize) -> bool {
        match cond {
            "library_count" => self.app.clips.len() >= n.max(1),
            "ingest_closed" => self.app.rx.is_none(),
            "selected_served" => self
                .app
                .live_sel
                .as_ref()
                .is_some_and(|l| l.first_frame.is_some()),
            "grid_settled" => !self.animating,
            "cache_thumbs" => self.probe.snapshot().thumbs_cached >= n as u64,
            _ => false,
        }
    }

    /// Resolve a target role to a tile's center in logical coordinates.
    fn target_center(&self, target: &str) -> Result<(f32, f32), String> {
        let n = self.app.clips.len();
        if n == 0 {
            return Err("no clips to target".into());
        }
        let i = match target {
            "first" | "" => 0,
            "last" => n - 1,
            "selected" => self.app.selected,
            t => t
                .strip_prefix("index:")
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&i| i < n)
                .ok_or_else(|| format!("bad target: {t}"))?,
        };
        let lay = self.app.layout();
        let (x, y, w, h) = self.app.tile_rect(&lay, i);
        Ok((x + w * 0.5, y + h * 0.5))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_inputs(setup: &Setup) -> Result<Vec<PathBuf>, String> {
    if !setup.inputs.is_empty() {
        return Ok(setup.inputs.iter().map(PathBuf::from).collect());
    }
    if setup.demo {
        return Ok(Vec::new());
    }
    let dir = std::env::var_os("SB_BENCH_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Default: benchmarks/fixtures relative to the repo root, which
            // is this crate's parent's parent.
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../benchmarks/fixtures")
        });
    let mut out = Vec::new();
    for f in &setup.fixtures {
        let p = dir.join(f);
        if !p.exists() {
            return Err(format!(
                "fixture not found: {} (generate with benchmarks/fixtures/generate.sh)",
                p.display()
            ));
        }
        out.push(p);
    }
    if out.is_empty() {
        return Err("setup has neither inputs nor fixtures nor demo".into());
    }
    Ok(out)
}

fn parse_key(k: &str) -> Option<Key> {
    Some(match k {
        "space" => Key::Space,
        "escape" | "esc" => Key::Escape,
        "enter" | "return" => Key::Enter,
        "tab" => Key::Tab,
        "left" => Key::Left,
        "right" => Key::Right,
        "up" => Key::Up,
        "down" => Key::Down,
        s if s.chars().count() == 1 => Key::Char(s.chars().next().unwrap()),
        _ => return None,
    })
}

/// Match spawn/ready/served/promotion events by lane generation and turn
/// them into the separate latency classes (phase-0-contracts §0.1) —
/// never pooled across classes.
fn compute_latencies(events: &[sb_media::RelEvent]) -> Vec<LatencyStat> {
    use std::collections::HashMap;
    #[derive(Default)]
    struct Lane {
        lane: String,
        spawn: Option<f64>,
        ready: Option<f64>,
        served: Option<f64>,
        promo: Option<f64>,
    }
    let mut by_gen: HashMap<u64, Lane> = HashMap::new();
    for e in events {
        let l = by_gen.entry(e.lane_gen).or_default();
        l.lane = e.lane.to_string();
        match e.kind {
            "decode_spawn" => l.spawn = Some(e.t),
            "decode_ready" => l.ready = l.ready.or(Some(e.t)),
            "frame_served" => l.served = l.served.or(Some(e.t)),
            "promotion" => l.promo = Some(e.t),
            _ => {}
        }
    }
    // (lane, metric) -> samples in ms
    let mut buckets: HashMap<(String, &'static str), Vec<f64>> = HashMap::new();
    for l in by_gen.values() {
        if let (Some(s), Some(r)) = (l.spawn, l.ready) {
            buckets
                .entry((l.lane.clone(), "spawn_to_ready"))
                .or_default()
                .push((r - s) * 1000.0);
        }
        if let (Some(s), Some(v)) = (l.spawn, l.served) {
            buckets
                .entry((l.lane.clone(), "spawn_to_served"))
                .or_default()
                .push((v - s) * 1000.0);
        }
        if let (Some(p), Some(v)) = (l.promo, l.served) {
            buckets
                .entry((l.lane.clone(), "promotion_to_served"))
                .or_default()
                .push((v - p) * 1000.0);
        }
    }
    let mut out: Vec<LatencyStat> = buckets
        .into_iter()
        .map(|((lane, metric), mut xs)| {
            let s = pctl_stat(&mut xs);
            LatencyStat {
                lane,
                metric: metric.to_string(),
                count: xs.len(),
                p50_ms: s.p50,
                p95_ms: s.p95,
                max_ms: s.max,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (a.lane.as_str(), a.metric.as_str()).cmp(&(b.lane.as_str(), b.metric.as_str()))
    });
    out
}

fn pctl_stat(xs: &mut [f64]) -> TickStat {
    if xs.is_empty() {
        return TickStat::default();
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    TickStat {
        p50: pctl(xs, 0.50),
        p95: pctl(xs, 0.95),
        max: *xs.last().unwrap(),
    }
}

/// Nearest-rank percentile of an already-sorted slice.
fn pctl(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn write_events_jsonl(out_dir: &Path, events: &[sb_media::RelEvent]) -> Result<(), String> {
    let mut s = String::with_capacity(events.len() * 96);
    for e in events {
        let line = sb_media::serde_json::to_string(e).map_err(|err| format!("event json: {err}"))?;
        s.push_str(&line);
        s.push('\n');
    }
    std::fs::write(out_dir.join("events.jsonl"), s).map_err(|e| format!("write events.jsonl: {e}"))
}

// ---------------------------------------------------------------------------
// Orchestration + reporting (Phase 3.5 / 4.1)
// ---------------------------------------------------------------------------

/// Read a `summary.json` written by [`run`].
pub fn read_summary(path: &Path) -> Result<Summary, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    sb_media::serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Run a scenario `reps` times, each as a fresh child process with an
/// isolated temp `HOME` (cold cache — the cache root is a process-global
/// `OnceLock`, so repeats MUST be separate processes), then write a
/// markdown report aggregating the per-rep summaries. Runs are serialized
/// (never parallel — ffmpeg contention skews timing). Returns the report
/// path.
///
/// `exe` is the path to this same `sb-bench` binary (its `run` subcommand
/// is spawned per rep). The per-rep run dirs and the report land under
/// `reports_root/<scenario>-<label>/`.
pub fn orchestrate(
    exe: &Path,
    scenario: &Path,
    reps: usize,
    label: &str,
    reports_root: &Path,
) -> Result<PathBuf, String> {
    let reps = reps.max(1);
    let stem = scenario
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("scenario");
    let bundle = reports_root.join(format!("{stem}-{label}"));
    std::fs::create_dir_all(&bundle).map_err(|e| format!("mkdir {}: {e}", bundle.display()))?;

    let mut summaries = Vec::new();
    for rep in 0..reps {
        let repdir = bundle.join(format!("rep{rep}"));
        let home = repdir.join("home");
        eprintln!("[orchestrate] rep {}/{}", rep + 1, reps);
        let status = Command::new(exe)
            .arg("run")
            .arg(scenario)
            .arg("--out")
            .arg(&repdir)
            .arg("--home")
            .arg(&home)
            .arg("--keep-home") // orchestrator owns cleanup below
            .status()
            .map_err(|e| format!("spawn rep {rep}: {e}"))?;
        let summary = read_summary(&repdir.join("summary.json"))
            .map_err(|e| format!("rep {rep}: {e} (child exit: {status})"))?;
        summaries.push(summary);
        // The per-rep cache (tens of MB of jpegs) is disposable; keep the
        // summary + events, drop the home so a large sweep doesn't balloon.
        let _ = std::fs::remove_dir_all(&home);
    }

    let md = markdown(&summaries, reps, label, &fingerprint());
    let report = bundle.join("report.md");
    std::fs::write(&report, md).map_err(|e| format!("write {}: {e}", report.display()))?;
    Ok(report)
}

/// (median, min, max) of a sample set, or all-zero when empty.
fn stat3(xs: &[f64]) -> (f64, f64, f64) {
    if xs.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (pctl(&v, 0.50), v[0], *v.last().unwrap())
}

fn fmt3(xs: &[f64], unit: &str) -> String {
    let (m, lo, hi) = stat3(xs);
    if xs.len() <= 1 {
        format!("{m:.0}{unit}")
    } else {
        format!("{m:.0}{unit} ({lo:.0}–{hi:.0})")
    }
}

/// Render the median±spread markdown report over `reps` summaries. Numbers
/// only — no verdicts; the scenario's prose intent is quoted at the top so
/// the reader (human or agent) can judge with full context.
pub fn markdown(summaries: &[Summary], reps: usize, label: &str, env: &str) -> String {
    use std::collections::BTreeMap;
    let mut out = String::new();
    let name = summaries.first().map(|s| s.scenario.as_str()).unwrap_or("?");
    let valid = summaries.iter().filter(|s| s.valid).count();

    out.push_str(&format!("# Benchmark report — `{name}` [{label}]\n\n"));
    out.push_str(&format!(
        "**{valid}/{reps} runs valid.** Median (min–max) across runs; numbers are \
         comparative — Tier A has no vsync present, so \"served on time\" is a proxy \
         for \"would have presented\". No verdicts here — judge against the intent.\n\n"
    ));
    out.push_str("## Intent\n\n");
    for line in summaries
        .first()
        .map(|s| s.intent.as_str())
        .unwrap_or("")
        .lines()
    {
        out.push_str(&format!("> {line}\n"));
    }
    out.push_str("\n## Environment\n\n");
    out.push_str(env);
    out.push('\n');

    // Latency: align (lane, metric) across reps, report median-of-p50 etc.
    out.push_str("\n## Latency (per lane / class)\n\n");
    out.push_str("| lane | metric | n (last run) | p50 ms | p95 ms | max ms |\n");
    out.push_str("|---|---|--:|--:|--:|--:|\n");
    let mut p50: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    let mut p95: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    let mut pmax: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    let mut lastn: BTreeMap<(String, String), usize> = BTreeMap::new();
    for s in summaries {
        for l in &s.latencies {
            let k = (l.lane.clone(), l.metric.clone());
            p50.entry(k.clone()).or_default().push(l.p50_ms);
            p95.entry(k.clone()).or_default().push(l.p95_ms);
            pmax.entry(k.clone()).or_default().push(l.max_ms);
            lastn.insert(k, l.count);
        }
    }
    for k in p50.keys() {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            k.0,
            k.1,
            lastn.get(k).copied().unwrap_or(0),
            fmt3(&p50[k], ""),
            fmt3(&p95[k], ""),
            fmt3(&pmax[k], ""),
        ));
    }

    // Counters + wall/frames.
    out.push_str("\n## Counters & totals\n\n");
    out.push_str("| metric | median (min–max) |\n|---|--:|\n");
    let col = |f: &dyn Fn(&Summary) -> f64| -> Vec<f64> { summaries.iter().map(f).collect() };
    let rows: [(&str, Vec<f64>); 8] = [
        ("wall_s", col(&|s| s.wall_s)),
        ("frames", col(&|s| s.frames as f64)),
        ("late_frames", col(&|s| s.counters.late_frames as f64)),
        ("reanchors", col(&|s| s.counters.reanchors as f64)),
        ("evictions", col(&|s| s.counters.evictions as f64)),
        ("thumbs_cached", col(&|s| s.counters.thumbs_cached as f64)),
        ("drain_budget_hits", col(&|s| s.counters.drain_budget_hits as f64)),
        ("tick_ms p95", col(&|s| s.tick_ms.p95)),
    ];
    for (name, xs) in &rows {
        let (m, lo, hi) = stat3(xs);
        let cell = if xs.len() <= 1 {
            format!("{m:.2}")
        } else {
            format!("{m:.2} ({lo:.2}–{hi:.2})")
        };
        out.push_str(&format!("| {name} | {cell} |\n"));
    }

    out.push_str("\nRaw per-run `summary.json` + `events.jsonl` are under `repN/` beside this report.\n");
    out
}

/// A one-block environment fingerprint (git, ffmpeg, platform). Best
/// effort — missing tools degrade to "unknown" rather than failing.
fn fingerprint() -> String {
    let cmd = |bin: &str, args: &[&str]| -> Option<String> {
        let o = Command::new(bin).args(args).output().ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let sha = cmd("git", &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = cmd("git", &["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let ffmpeg = cmd("ffmpeg", &["-hide_banner", "-version"])
        .and_then(|s| s.lines().next().map(str::to_string))
        .unwrap_or_else(|| "unknown".into());
    let host = cmd("hostname", &[]).unwrap_or_else(|| "unknown".into());
    format!(
        "- git: `{sha}`{}\n- ffmpeg: `{ffmpeg}`\n- platform: `{} {}`\n- host: `{host}`\n",
        if dirty { " (dirty tree)" } else { "" },
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(scenario: &str, valid: bool, p50: f64, late: u64) -> Summary {
        Summary {
            scenario: scenario.into(),
            intent: "line one\nline two".into(),
            valid,
            invalid_reasons: vec![],
            wall_s: 6.0,
            frames: 200,
            counters: sb_media::CounterSnapshot {
                late_frames: late,
                thumbs_cached: 1,
                ..Default::default()
            },
            latencies: vec![LatencyStat {
                lane: "selected".into(),
                metric: "spawn_to_served".into(),
                count: 1,
                p50_ms: p50,
                p95_ms: p50,
                max_ms: p50,
            }],
            thumbs_curve: vec![[0.0, 0.0], [0.3, 1.0]],
            tick_ms: TickStat {
                p50: 0.05,
                p95: 0.1,
                max: 0.2,
            },
            events: 3,
            events_dropped: 0,
            conditions: vec![],
        }
    }

    /// The markdown report aggregates median (min–max) across reps, quotes
    /// the intent, and never emits a verdict.
    #[test]
    fn markdown_aggregates_reps_with_spread() {
        let reps = vec![
            summary("s", true, 250.0, 0),
            summary("s", true, 260.0, 0),
            summary("s", false, 300.0, 4),
        ];
        let md = markdown(&reps, 3, "baseline", "- git: `abc`\n");
        assert!(md.contains("2/3 runs valid"));
        assert!(md.contains("> line one"), "intent quoted");
        // Median of {250,260,300} = 260, spread 250–300.
        assert!(md.contains("260 (250–300)"), "latency median+spread:\n{md}");
        // late_frames median of {0,0,4} = 0, spread 0–4.
        assert!(md.contains("0.00 (0.00–4.00)"), "counter median+spread");
        assert!(!md.to_lowercase().contains("regression"), "no verdicts");
    }

    /// End-to-end: a tiny scenario against a real fixture produces a valid
    /// summary with the selected lane's spawn→served latency recorded.
    /// Skips cleanly when the fixture corpus hasn't been generated.
    #[test]
    fn runs_a_scenario_and_measures_selected_latency() {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../benchmarks/fixtures/h264_1080p30.mp4");
        if !fixtures.exists() {
            eprintln!("skipping: run benchmarks/fixtures/generate.sh first");
            return;
        }
        let dir = std::env::temp_dir().join("sb_bench_selftest");
        let _ = std::fs::create_dir_all(&dir);
        let scenario = dir.join("s.toml");
        std::fs::write(
            &scenario,
            r#"
name = "selftest_open_quickview"
intent = "Open the first clip; the selected stream should serve a frame."

[setup]
inputs = ["FIXTURE"]
animation = "normal"
viewport = [1280, 800]

[[step]]
action = "wait_until"
cond = "library_count"
n = 1
timeout = 10.0

[[step]]
action = "key"
key = "space"

[[step]]
action = "wait_until"
cond = "selected_served"
timeout = 15.0

[[step]]
action = "wait"
secs = 0.5

[validity]
require = ["selected_served"]
"#
            .replace("FIXTURE", fixtures.to_str().unwrap()),
        )
        .unwrap();

        let out = dir.join("out");
        let summary = run(&scenario, &out).expect("runner completes");
        assert!(
            summary.valid,
            "run should be valid, reasons: {:?}",
            summary.invalid_reasons
        );
        assert!(summary.events > 0, "some events were recorded");
        assert!(out.join("events.jsonl").exists());
        assert!(out.join("summary.json").exists());
        // The selected lane spawned and served a frame.
        let served = summary
            .latencies
            .iter()
            .find(|l| l.lane == "selected" && l.metric == "spawn_to_served");
        assert!(served.is_some(), "selected spawn_to_served recorded");
    }
}
