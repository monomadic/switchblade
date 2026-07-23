//! Tier-A headless benchmark runner (benchmarks/HARNESS.md Phase 3;
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
    /// Feel-constant overrides for this run. A partial `[tuning]` table —
    /// `Tuning`'s `#[serde(default)]` fills every unspecified field — so a
    /// scenario can sweep a knob (e.g. `live_delay_ms`) without a rebuild.
    /// `sb-bench run --set k=v` overlays onto this table.
    #[serde(default)]
    pub tuning: crate::Tuning,
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
    /// `pinch`: magnification delta per event (winit's units; positive =
    /// fingers apart). Repeated `n` times, one frame each, so a scenario
    /// can drive a real gesture rather than a single jump.
    #[serde(default)]
    pub delta: f32,
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
    /// The `--set k=v` tuning overrides applied to this run (empty when a
    /// bare scenario ran). Self-documents A/B knob sweeps in the report.
    #[serde(default)]
    pub tuning_sets: Vec<String>,
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

    // ── slow-disk / big-library instruments (perf-review 03) ──────────
    /// How many `frame()` calls blew past each stall threshold. A p95 of
    /// 3ms hides the four 900ms frames that are the entire complaint, so
    /// the tail gets counted explicitly rather than summarized away.
    #[serde(default)]
    pub tick_over: Vec<TickOver>,
    /// Per-phase render-thread cost, ms per tick (from the phase
    /// counters): which part of `frame()` a stall belongs to.
    #[serde(default)]
    pub phase_ms: Vec<PhaseStat>,
    /// Scheduler state sampled per tick, compacted to changes:
    /// [t, thumbs, gen, anims, inflight, gen_running, pending_results].
    #[serde(default)]
    pub queue_curve: Vec<[f64; 7]>,
    /// Per-tier worker-job cost from the JobStart/JobEnd pairs, split by
    /// outcome (a cache hit never touched the source drive).
    #[serde(default)]
    pub jobs: Vec<JobStat>,
    /// Fraction of wall time × WORKERS that workers spent inside jobs.
    /// Low utilisation with a deep queue = the pool is parked on a
    /// throttle; ~1.0 = saturated.
    #[serde(default)]
    pub worker_utilisation: f64,
    /// Process-tree resident memory + thread count, sampled off the
    /// render thread (see `sample_process`): [t, rss_mb, threads,
    /// children]. The crash canary — an unbounded backlog or a thread
    /// leak shows here before anything else notices.
    #[serde(default)]
    pub proc_curve: Vec<[f64; 4]>,
    /// Longest gap between consecutive served frames per live lane, ms —
    /// the video-thread stall metric. A decoder starved by drive
    /// contention shows up here as a multi-second gap while the app's own
    /// tick times stay fine.
    #[serde(default)]
    pub frame_gap_ms: Vec<LatencyStat>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TickOver {
    pub over_ms: f64,
    pub count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PhaseStat {
    pub phase: String,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    /// Share of total measured render-thread time this phase accounts for.
    pub share: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobStat {
    pub tier: String,
    pub outcome: String,
    pub count: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    /// Total worker-seconds this (tier, outcome) population consumed.
    pub total_s: f64,
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
/// `events.jsonl` under `out_dir`, and return the summary. `sets` are
/// `key=value` tuning overrides overlaid onto the scenario's `[tuning]`
/// table (knob sweeps without editing the file). The caller is responsible
/// for hermetic isolation (a fresh temp `HOME` per run, set BEFORE this is
/// called — the cache root is a process-global `OnceLock`).
pub fn run(scenario_path: &Path, out_dir: &Path, sets: &[String]) -> Result<Summary, String> {
    let text = std::fs::read_to_string(scenario_path)
        .map_err(|e| format!("read {}: {e}", scenario_path.display()))?;
    let scenario = parse_scenario(&text, sets)?;
    std::fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {}: {e}", out_dir.display()))?;

    let inputs = resolve_inputs(&scenario.setup)?;
    let opts = Options {
        animation: Some(AnimLevel::parse(&scenario.setup.animation).unwrap_or(AnimLevel::Normal)),
        inputs,
        demo: scenario.setup.demo,
        no_config: true, // hermetic: a stray config must never steer a bench
        tuning: Some(scenario.tuning.clone()),
        ..Options::default()
    };
    let viewport = Viewport {
        width: scenario.setup.viewport[0],
        height: scenario.setup.viewport[1],
    };

    // Process-tree canary, off the render thread (see `sample_process`).
    // 1Hz: the numbers it watches (RSS growth, thread leaks) move over
    // seconds, and forking `ps` more often would itself be load.
    let proc_curve = std::sync::Arc::new(std::sync::Mutex::new(Vec::<[f64; 4]>::new()));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (curve, stop, pid) = (proc_curve.clone(), stop.clone(), std::process::id());
        let t0 = Instant::now();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some([rss, threads, kids]) = sample_process(pid) {
                    curve
                        .lock()
                        .unwrap()
                        .push([t0.elapsed().as_secs_f64(), rss, threads, kids]);
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

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
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (events, dropped) = rig.probe.drain(rig.anchor);
    write_events_jsonl(out_dir, &events)?;

    // Mechanical validity: every step ran (no early break) + required
    // conditions all met. NEVER a performance verdict.
    for req in &scenario.validity.require {
        if !rig.conds.iter().any(|(c, _)| c == req) {
            invalid.push(format!("required condition never met: {req}"));
        }
    }
    // Surface EVERY recorded condition (each `wait_until` records its
    // met-time, not only the validity-required ones), so diagnostic
    // brackets like the anim-sheet lifecycle appear in the summary with
    // timings. Required-but-never-met conds still show, flagged unmet.
    let mut conditions: Vec<CondResult> = rig
        .conds
        .iter()
        .map(|(cond, t)| CondResult {
            cond: cond.clone(),
            met: true,
            at_s: Some(*t),
        })
        .collect();
    for req in &scenario.validity.require {
        if !rig.conds.iter().any(|(c, _)| c == req) {
            conditions.push(CondResult {
                cond: req.clone(),
                met: false,
                at_s: None,
            });
        }
    }

    let summary = Summary {
        scenario: scenario.name,
        intent: scenario.intent,
        tuning_sets: sets.to_vec(),
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
        tick_over: [16.0, 50.0, 100.0, 250.0, 1000.0]
            .iter()
            .map(|&over_ms| TickOver {
                over_ms,
                count: rig.tick_ms.iter().filter(|&&t| t > over_ms).count(),
            })
            .collect(),
        phase_ms: compute_phases(&rig.phase_ms),
        queue_curve: rig.queue_curve.clone(),
        jobs: compute_jobs(&events),
        worker_utilisation: {
            let busy_s = rig.probe.snapshot().worker_busy_us as f64 / 1e6;
            busy_s / (wall_s.max(1e-9) * MEDIA_WORKERS as f64)
        },
        proc_curve: proc_curve.lock().unwrap().clone(),
        frame_gap_ms: compute_frame_gaps(&events),
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
    /// Per-frame per-phase cost in ms, from the phase counters' deltas.
    phase_ms: Vec<(&'static str, Vec<f64>)>,
    /// Previous tick's phase counter reads, for those deltas.
    last_phase: [u64; 3],
    /// Scheduler state, compacted to changes (see `Summary::queue_curve`).
    queue_curve: Vec<[f64; 7]>,
    last_depths: Option<sb_media::QueueDepths>,
    /// First time each named condition became true (name, t_seconds).
    conds: Vec<(String, f64)>,
}

/// Process-tree resident memory + thread count, sampled from a helper
/// thread so the render thread never pays for it. Uses `ps` rather than a
/// new libc dependency: the ffmpeg workers are separate processes, so the
/// number that matters is the whole tree's, and one fork per second off
/// the measured thread is cheaper than the alternative is worth.
fn sample_process(pid: u32) -> Option<[f64; 3]> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let rss_kb: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    // macOS `ps` has no `thcount`/`nlwp` keyword — `-M` lists one row per
    // thread under a header, so the count is rows-minus-one. The thread
    // canary matters here: a stalled reader that never gets dropped leaks
    // a thread pinning ~30MB (docs/architecture/live-playback.md).
    let threads = Command::new("ps")
        .args(["-M", "-p", &pid.to_string()])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .count()
                .saturating_sub(1) as f64
        })
        .unwrap_or(0.0);
    // ffmpeg workers are children; their RSS is real memory this run is
    // responsible for even though it isn't ours.
    let kids = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .ok();
    let mut child_rss = 0.0;
    let mut child_n = 0.0;
    if let Some(k) = kids {
        for p in String::from_utf8_lossy(&k.stdout).split_whitespace() {
            child_n += 1.0;
            if let Some(o) = Command::new("ps")
                .args(["-o", "rss=", "-p", p])
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok())
            {
                child_rss += o;
            }
        }
    }
    Some([(rss_kb + child_rss) / 1024.0, threads, child_n])
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
            phase_ms: vec![
                ("ingest_install", Vec::new()),
                ("drain_media", Vec::new()),
                ("live", Vec::new()),
            ],
            last_phase: [0; 3],
            queue_curve: Vec::new(),
            last_depths: None,
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
        let snap = self.probe.snapshot();
        // Compact thumbs-cached curve: a point only when it changes.
        if snap.thumbs_cached != self.last_thumbs {
            self.last_thumbs = snap.thumbs_cached;
            self.thumbs_curve
                .push([self.anchor.elapsed().as_secs_f64(), snap.thumbs_cached as f64]);
        }
        // Phase attribution: the counters are cumulative, so this tick's
        // cost per phase is the delta since the last one.
        let now_phase = [snap.ns_ingest_install, snap.ns_drain_media, snap.ns_live];
        for (i, (_, xs)) in self.phase_ms.iter_mut().enumerate() {
            xs.push(now_phase[i].saturating_sub(self.last_phase[i]) as f64 / 1e6);
        }
        self.last_phase = now_phase;
        // Scheduler state, compacted to changes — a 19k-file sweep would
        // otherwise write one identical row per frame for minutes.
        let d = self.app.media.depths();
        if self.last_depths != Some(d) {
            self.last_depths = Some(d);
            self.queue_curve.push([
                self.anchor.elapsed().as_secs_f64(),
                d.thumbs as f64,
                d.gen_sweep as f64,
                d.anims as f64,
                d.inflight as f64,
                d.gen_running as f64,
                snap.pending_results as f64,
            ]);
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
            "pinch" => {
                // A trackpad pinch is a STREAM of small deltas, one per
                // frame — the shape that matters for the ribbon, since
                // each event re-lays the gesture.
                for _ in 0..step.n.max(1) {
                    self.app.event(InputEvent::Pinch { delta: step.delta });
                    self.tick();
                }
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
            // The grid/filmstrip hover lane's first frame (its FrameServed
            // event carries the served pts — the handoff diagnostic).
            "hover_served" => self
                .app
                .live_hover
                .as_ref()
                .is_some_and(|l| l.first_frame.is_some()),
            "grid_settled" => !self.animating,
            "cache_thumbs" => self.probe.snapshot().thumbs_cached >= n as u64,
            // Background-sweep progress, in the app's own ledger terms —
            // `cache_thumbs` only counts clips that reached the grid as
            // cached, while this counts every finished job (hits, misses
            // and failures alike). On a big library the two diverge, and
            // the difference is itself diagnostic.
            "jobs_done" => self.app.jobs_done >= n as u64,
            "sweep_drained" => {
                self.app.rx.is_none() && self.app.jobs_done >= self.app.jobs_total
            }
            // Anim-sheet (storyboard) lifecycle for the SELECTED clip — the
            // sheet whose cells the seekbar hover and chapter-bar chips
            // sample. `requested` leaves Thumb::None the moment
            // request_quickview_sheet fires (past the prewarm_ok /
            // warm_filling gates); `selected_anim` is Ready, i.e. decoded
            // AND atlas-resident, the point the chips can actually draw.
            // The two bracket the delay: requested-since-served = the gate
            // cascade cost, ready-since-requested = gen + install cost.
            "selected_anim_requested" => self
                .app
                .clips
                .get(self.app.selected)
                .is_some_and(|c| !matches!(c.anim, crate::Thumb::None)),
            "selected_anim" => self
                .app
                .clips
                .get(self.app.selected)
                .is_some_and(|c| matches!(c.anim, crate::Thumb::Ready { .. })),
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

/// Parse a scenario, overlaying `key=value` `--set` overrides onto its
/// `[tuning]` table before deserializing. Values are written as TOML:
/// `true`/`false` and anything parsing as a number pass through verbatim
/// (so **floats need a decimal point** — `live_delay_ms=250.0`), and
/// everything else is quoted as a string (`grid_layout=flexible`).
fn parse_scenario(text: &str, sets: &[String]) -> Result<Scenario, String> {
    if sets.is_empty() {
        return toml::from_str(text).map_err(|e| format!("parse scenario: {e}"));
    }
    let mut doc: toml::Table = toml::from_str(text).map_err(|e| format!("parse scenario: {e}"))?;
    // Build a `[tuning]` patch from the k=v pairs, then merge it in.
    let mut patch_src = String::new();
    for kv in sets {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("--set expects key=value, got {kv:?}"))?;
        let rhs = if v == "true" || v == "false" || v.parse::<f64>().is_ok() {
            v.to_string()
        } else {
            format!("{v:?}") // quote as a TOML string
        };
        patch_src.push_str(&format!("{k} = {rhs}\n"));
    }
    let patch: toml::Table =
        toml::from_str(&patch_src).map_err(|e| format!("bad --set value: {e}"))?;
    let tuning = doc
        .entry("tuning".to_string())
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let tbl = tuning
        .as_table_mut()
        .ok_or_else(|| "scenario [tuning] is not a table".to_string())?;
    for (k, v) in patch {
        tbl.insert(k, v);
    }
    doc.try_into().map_err(|e| format!("parse scenario: {e}"))
}

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

/// The worker-pool width, mirrored for the utilisation denominator. Not
/// public in sb-media; kept here so a change there shows up as a wrong
/// utilisation number rather than a compile error nobody notices.
const MEDIA_WORKERS: usize = 3;

/// Per-phase render-thread percentiles + each phase's share of the
/// measured total. Shares are of *attributed* time only — the rest of
/// `frame()` (layout, springs, frame build) is unattributed by design;
/// these three are the phases that touch media and ingest.
fn compute_phases(phases: &[(&'static str, Vec<f64>)]) -> Vec<PhaseStat> {
    let total: f64 = phases.iter().map(|(_, xs)| xs.iter().sum::<f64>()).sum();
    phases
        .iter()
        .map(|(name, xs)| {
            let sum: f64 = xs.iter().sum();
            let s = pctl_stat(&mut xs.clone());
            PhaseStat {
                phase: name.to_string(),
                p50_ms: s.p50,
                p95_ms: s.p95,
                max_ms: s.max,
                share: if total > 0.0 { sum / total } else { 0.0 },
            }
        })
        .collect()
}

/// Worker-job cost per (tier, outcome), from the `JobEnd` events' own
/// `ms`. Split by outcome because a cache hit and a cold 4K extract are
/// different populations — pooling them makes a slow-disk run look fast
/// in exactly the case where it isn't.
fn compute_jobs(events: &[sb_media::RelEvent]) -> Vec<JobStat> {
    use std::collections::HashMap;
    let mut buckets: HashMap<(String, String), Vec<f64>> = HashMap::new();
    for e in events.iter().filter(|e| e.kind == "job_end") {
        let (Some(tier), Some(ms)) = (e.tier, e.ms) else {
            continue;
        };
        buckets
            .entry((tier.to_string(), e.outcome.unwrap_or("?").to_string()))
            .or_default()
            .push(ms);
    }
    let mut out: Vec<JobStat> = buckets
        .into_iter()
        .map(|((tier, outcome), mut xs)| {
            let total_s = xs.iter().sum::<f64>() / 1000.0;
            let count = xs.len();
            let s = pctl_stat(&mut xs);
            JobStat {
                tier,
                outcome,
                count,
                p50_ms: s.p50,
                p95_ms: s.p95,
                max_ms: s.max,
                total_s,
            }
        })
        .collect();
    // Heaviest population first — that's the one holding the pool.
    out.sort_by(|a, b| b.total_s.partial_cmp(&a.total_s).unwrap());
    out
}

/// Gaps between consecutive `frame_served` events per lane incarnation,
/// in ms. A live decoder starved by drive contention produces a long gap
/// here while `tick_ms` stays clean — the app is fine, the video is not.
/// Gaps are computed WITHIN a lane_gen: a respawn is not a stall.
fn compute_frame_gaps(events: &[sb_media::RelEvent]) -> Vec<LatencyStat> {
    use std::collections::HashMap;
    let mut last: HashMap<u64, f64> = HashMap::new();
    let mut by_lane: HashMap<String, Vec<f64>> = HashMap::new();
    for e in events.iter().filter(|e| e.kind == "frame_served") {
        if let Some(prev) = last.insert(e.lane_gen, e.t) {
            by_lane
                .entry(e.lane.to_string())
                .or_default()
                .push((e.t - prev) * 1000.0);
        }
    }
    let mut out: Vec<LatencyStat> = by_lane
        .into_iter()
        .map(|(lane, mut xs)| {
            let count = xs.len();
            let s = pctl_stat(&mut xs);
            LatencyStat {
                lane,
                metric: "frame_gap".into(),
                count,
                p50_ms: s.p50,
                p95_ms: s.p95,
                max_ms: s.max,
            }
        })
        .collect();
    out.sort_by(|a, b| a.lane.cmp(&b.lane));
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

/// Read every rep's `summary.json` from an orchestration bundle dir
/// (`repN/summary.json`).
pub fn read_bundle(dir: &Path) -> Result<Vec<Summary>, String> {
    let mut out = Vec::new();
    for rep in 0.. {
        let p = dir.join(format!("rep{rep}")).join("summary.json");
        if !p.exists() {
            break;
        }
        out.push(read_summary(&p)?);
    }
    if out.is_empty() {
        return Err(format!("no rep*/summary.json under {}", dir.display()));
    }
    Ok(out)
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
    sets: &[String],
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
        let mut cmd = Command::new(exe);
        cmd.arg("run")
            .arg(scenario)
            .arg("--out")
            .arg(&repdir)
            .arg("--home")
            .arg(&home)
            .arg("--keep-home"); // orchestrator owns cleanup below
        for kv in sets {
            cmd.arg("--set").arg(kv);
        }
        let status = cmd.status().map_err(|e| format!("spawn rep {rep}: {e}"))?;
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

/// (mean, min, max) of a sample set, or all-zero when empty. Used for
/// **burst counters** (late_frames, reanchors, evictions): those are mostly
/// zero with occasional spikes, and the spikes ARE the signal — the median
/// discards them (reports 0), so mean is the honest aggregate. Latency
/// percentiles keep the outlier-robust median-of-p50 via `stat3`.
fn mean3(xs: &[f64]) -> (f64, f64, f64) {
    if xs.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mean = xs.iter().sum::<f64>() / xs.len() as f64;
    let lo = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    (mean, lo, hi)
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
    if let Some(sets) = summaries.first().map(|s| &s.tuning_sets)
        && !sets.is_empty()
    {
        out.push_str(&format!("**Tuning overrides:** `{}`\n\n", sets.join("`, `")));
    }
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

    // Counters + wall/frames. Burst counters (late_frames, reanchors,
    // evictions) aggregate by MEAN — they're mostly zero with occasional
    // spikes, and a median would report 0 and hide the spikes that are the
    // whole point. Steady per-run measures (wall/frames/tick) keep the median.
    out.push_str("\n## Counters & totals\n\n");
    out.push_str("| metric | agg (min–max) | agg |\n|---|--:|:--|\n");
    let col = |f: &dyn Fn(&Summary) -> f64| -> Vec<f64> { summaries.iter().map(f).collect() };
    let rows: [(&str, Vec<f64>, bool); 20] = [
        ("wall_s", col(&|s| s.wall_s), false),
        ("frames", col(&|s| s.frames as f64), false),
        ("late_frames", col(&|s| s.counters.late_frames as f64), true),
        ("reanchors", col(&|s| s.counters.reanchors as f64), true),
        ("evictions", col(&|s| s.counters.evictions as f64), true),
        ("thumbs_cached", col(&|s| s.counters.thumbs_cached as f64), false),
        ("drain_budget_hits", col(&|s| s.counters.drain_budget_hits as f64), false),
        ("tick_ms p50", col(&|s| s.tick_ms.p50), false),
        ("tick_ms p95", col(&|s| s.tick_ms.p95), false),
        ("tick_ms max", col(&|s| s.tick_ms.max), true),
        // Scheduler / pool.
        ("jobs_started", col(&|s| s.counters.jobs_started as f64), false),
        ("jobs_finished", col(&|s| s.counters.jobs_finished as f64), false),
        ("jobs_hit", col(&|s| s.counters.jobs_hit as f64), false),
        ("jobs_failed", col(&|s| s.counters.jobs_failed as f64), true),
        ("worker_utilisation", col(&|s| s.worker_utilisation), false),
        // Backlog / memory canary.
        ("pending_bytes_peak_mb", col(&|s| s.counters.pending_bytes_peak as f64 / 1048576.0), true),
        // Ingest.
        ("ingest_seen", col(&|s| s.counters.ingest_seen as f64), false),
        ("ingest_rejected", col(&|s| s.counters.ingest_rejected as f64), false),
        ("ingest_io_s", col(&|s| s.counters.ingest_io_us as f64 / 1e6), false),
        ("rss_peak_mb", col(&|s| {
            s.proc_curve.iter().map(|p| p[1]).fold(0.0, f64::max)
        }), true),
    ];
    for (name, xs, burst) in &rows {
        let (m, lo, hi) = if *burst { mean3(xs) } else { stat3(xs) };
        let how = if *burst { "mean" } else { "median" };
        let cell = if xs.len() <= 1 {
            format!("{m:.2}")
        } else {
            format!("{m:.2} ({lo:.2}–{hi:.2})")
        };
        out.push_str(&format!("| {name} | {cell} | {how} |\n"));
    }

    // Stall tail. Percentiles hide it by construction; the counts don't.
    out.push_str("\n## Render-thread stalls (frames over threshold)\n\n");
    out.push_str("| > ms | count (mean across runs) |\n|--:|--:|\n");
    let mut overs: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for s in summaries {
        for t in &s.tick_over {
            overs
                .entry(format!("{:>6.0}", t.over_ms))
                .or_default()
                .push(t.count as f64);
        }
    }
    for (k, xs) in &overs {
        out.push_str(&format!("| {} | {:.1} |\n", k.trim(), mean3(xs).0));
    }

    // Which phase of frame() owns the time.
    out.push_str("\n## Render-thread phase attribution\n\n");
    out.push_str("| phase | p50 ms | p95 ms | max ms | share |\n|---|--:|--:|--:|--:|\n");
    let mut ph: BTreeMap<String, (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for s in summaries {
        for p in &s.phase_ms {
            let e = ph.entry(p.phase.clone()).or_default();
            e.0.push(p.p50_ms);
            e.1.push(p.p95_ms);
            e.2.push(p.max_ms);
            e.3.push(p.share);
        }
    }
    for (k, (a, b, c, d)) in &ph {
        out.push_str(&format!(
            "| {k} | {} | {} | {} | {:.0}% |\n",
            fmt3(a, ""),
            fmt3(b, ""),
            fmt3(c, ""),
            stat3(d).0 * 100.0
        ));
    }

    // Worker-pool occupancy per tier. `total_s` is what holds the pool.
    out.push_str("\n## Worker jobs (per tier / outcome)\n\n");
    out.push_str("| tier | outcome | n | p50 ms | p95 ms | max ms | worker-s |\n");
    out.push_str("|---|---|--:|--:|--:|--:|--:|\n");
    let mut jb: BTreeMap<(String, String), (usize, Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>)> =
        BTreeMap::new();
    for s in summaries {
        for j in &s.jobs {
            let e = jb
                .entry((j.tier.clone(), j.outcome.clone()))
                .or_insert_with(|| (0, vec![], vec![], vec![], vec![]));
            e.0 = j.count;
            e.1.push(j.p50_ms);
            e.2.push(j.p95_ms);
            e.3.push(j.max_ms);
            e.4.push(j.total_s);
        }
    }
    for ((tier, outcome), (n, a, b, c, d)) in &jb {
        out.push_str(&format!(
            "| {tier} | {outcome} | {n} | {} | {} | {} | {} |\n",
            fmt3(a, ""),
            fmt3(b, ""),
            fmt3(c, ""),
            fmt3(d, ""),
        ));
    }

    // The video-thread view: gaps between served frames, within a lane
    // incarnation. A stall here is invisible in tick_ms.
    if summaries.iter().any(|s| !s.frame_gap_ms.is_empty()) {
        out.push_str("\n## Served-frame gaps (video-thread stalls)\n\n");
        out.push_str("| lane | n | p50 ms | p95 ms | max ms |\n|---|--:|--:|--:|--:|\n");
        let mut fg: BTreeMap<String, (usize, Vec<f64>, Vec<f64>, Vec<f64>)> = BTreeMap::new();
        for s in summaries {
            for g in &s.frame_gap_ms {
                let e = fg.entry(g.lane.clone()).or_default();
                e.0 = g.count;
                e.1.push(g.p50_ms);
                e.2.push(g.p95_ms);
                e.3.push(g.max_ms);
            }
        }
        for (lane, (n, a, b, c)) in &fg {
            out.push_str(&format!(
                "| {lane} | {n} | {} | {} | {} |\n",
                fmt3(a, ""),
                fmt3(b, ""),
                fmt3(c, ""),
            ));
        }
    }

    out.push_str("\nRaw per-run `summary.json` + `events.jsonl` are under `repN/` beside this report.\n");
    out
}

/// A `(lane, metric)` latency key.
type LaneKey = (String, String);

/// A compare-table counter column: label, extractor, and whether it's a burst
/// counter (mean-aggregated) rather than a steady per-run measure (median).
type CounterCol = (&'static str, fn(&Summary) -> f64, bool);

/// Median-of-p50 for each `(lane, metric)` across a run set.
fn latency_medians(runs: &[Summary]) -> std::collections::BTreeMap<LaneKey, f64> {
    use std::collections::BTreeMap;
    let mut by: BTreeMap<LaneKey, Vec<f64>> = BTreeMap::new();
    for s in runs {
        for l in &s.latencies {
            by.entry((l.lane.clone(), l.metric.clone()))
                .or_default()
                .push(l.p50_ms);
        }
    }
    by.into_iter().map(|(k, xs)| (k, stat3(&xs).0)).collect()
}

/// Side-by-side compare of two labeled run sets (e.g. before/after a
/// change). Reports each side's median and the B−A delta. **Numbers only,
/// no verdict** — the reader decides whether a delta helped, with the
/// shared intent for context. Interleave the two `bench` invocations
/// (A, B, A, B, …) to dodge thermal drift before reading this.
pub fn compare_markdown(a: &[Summary], b: &[Summary], la: &str, lb: &str) -> String {
    let mut out = String::new();
    let name = a.first().map(|s| s.scenario.as_str()).unwrap_or("?");
    out.push_str(&format!("# Benchmark compare — `{name}`: {la} vs {lb}\n\n"));
    out.push_str(&format!(
        "Median across runs; **delta = {lb} − {la}** (negative = {lb} lower). \
         Comparative, no verdict — judge against the intent. A and B should be \
         run interleaved to dodge thermal drift.\n\n"
    ));
    let sets = |runs: &[Summary]| {
        runs.first()
            .map(|s| s.tuning_sets.join("`, `"))
            .filter(|s| !s.is_empty())
            .map(|s| format!("`{s}`"))
            .unwrap_or_else(|| "(none)".into())
    };
    out.push_str(&format!(
        "- {la} tuning: {}\n- {lb} tuning: {}\n\n",
        sets(a),
        sets(b)
    ));
    out.push_str("## Intent\n\n");
    for line in a.first().map(|s| s.intent.as_str()).unwrap_or("").lines() {
        out.push_str(&format!("> {line}\n"));
    }

    out.push_str("\n## Latency p50 (ms)\n\n");
    out.push_str(&format!("| lane / metric | {la} | {lb} | Δ |\n|---|--:|--:|--:|\n"));
    let (ma, mb) = (latency_medians(a), latency_medians(b));
    let mut keys: Vec<_> = ma.keys().chain(mb.keys()).cloned().collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        let va = ma.get(&k).copied();
        let vb = mb.get(&k).copied();
        let delta = match (va, vb) {
            (Some(x), Some(y)) => format!("{:+.0}", y - x),
            _ => "—".into(),
        };
        out.push_str(&format!(
            "| {} / {} | {} | {} | {} |\n",
            k.0,
            k.1,
            va.map(|x| format!("{x:.0}")).unwrap_or_else(|| "—".into()),
            vb.map(|x| format!("{x:.0}")).unwrap_or_else(|| "—".into()),
            delta,
        ));
    }

    // Burst counters aggregate by MEAN (spikes are the signal; a median
    // would report 0 and hide them); steady measures keep the median.
    out.push_str("\n## Counters & totals\n\n");
    out.push_str(&format!("| metric | {la} | {lb} | Δ | agg |\n|---|--:|--:|--:|:--|\n"));
    let cols: [CounterCol; 6] = [
        ("wall_s", |s| s.wall_s, false),
        ("late_frames", |s| s.counters.late_frames as f64, true),
        ("reanchors", |s| s.counters.reanchors as f64, true),
        ("evictions", |s| s.counters.evictions as f64, true),
        ("thumbs_cached", |s| s.counters.thumbs_cached as f64, false),
        ("tick_ms p95", |s| s.tick_ms.p95, false),
    ];
    for (label, f, burst) in cols {
        let agg = |runs: &[Summary]| -> f64 {
            let xs: Vec<f64> = runs.iter().map(f).collect();
            if burst { mean3(&xs).0 } else { stat3(&xs).0 }
        };
        let (xa, xb) = (agg(a), agg(b));
        let how = if burst { "mean" } else { "median" };
        out.push_str(&format!(
            "| {label} | {xa:.2} | {xb:.2} | {:+.2} | {how} |\n",
            xb - xa
        ));
    }
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
            tuning_sets: vec![],
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
            tick_over: vec![TickOver {
                over_ms: 100.0,
                count: 2,
            }],
            phase_ms: vec![PhaseStat {
                phase: "drain_media".into(),
                p50_ms: 0.02,
                p95_ms: 0.4,
                max_ms: 1.0,
                share: 0.5,
            }],
            queue_curve: vec![],
            jobs: vec![JobStat {
                tier: "gen".into(),
                outcome: "made".into(),
                count: 4,
                p50_ms: 900.0,
                p95_ms: 2000.0,
                max_ms: 2500.0,
                total_s: 5.0,
            }],
            worker_utilisation: 0.9,
            proc_curve: vec![[0.0, 120.0, 14.0, 1.0]],
            frame_gap_ms: vec![],
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
        // late_frames is a BURST counter → mean of {0,0,4} = 1.33 (a median
        // would report 0 and hide the spike). Spread still 0–4.
        assert!(md.contains("1.33 (0.00–4.00)"), "burst counter mean+spread:\n{md}");
        assert!(!md.to_lowercase().contains("regression"), "no verdicts");
    }

    /// Burst counters must aggregate by MEAN, not median: a run set that is
    /// mostly zero with a couple of spikes (the disk-contention signature)
    /// would read as "0" under a median and hide the effect entirely.
    #[test]
    fn burst_counters_report_mean_not_median() {
        // late_frames = {0, 0, 0, 16, 0, 9}: median 0, mean 4.17.
        let lates = [0u64, 0, 0, 16, 0, 9];
        let reps: Vec<Summary> = lates.iter().map(|&l| summary("s", true, 300.0, l)).collect();
        let md = markdown(&reps, reps.len(), "cold", "- git: `abc`\n");
        assert!(
            md.contains("| late_frames | 4.17 (0.00–16.00) | mean |"),
            "burst counter must surface the spike via mean:\n{md}"
        );
        // Compare must show the mean delta, not a median 0−0=0.
        let quiet: Vec<Summary> = (0..6).map(|_| summary("s", true, 300.0, 0)).collect();
        let cmp = compare_markdown(&quiet, &reps, "capped", "uncapped");
        assert!(
            cmp.contains("| late_frames | 0.00 | 4.17 | +4.17 | mean |"),
            "compare must surface the burst delta, not a median 0:\n{cmp}"
        );
    }

    /// `--set` overrides overlay onto the scenario's `[tuning]` table, with
    /// the documented value coercion (int/float/bool bare, strings quoted).
    #[test]
    fn set_overrides_patch_the_tuning_table() {
        let text = r#"
name = "s"
[setup]
demo = true
[tuning]
live_delay_ms = 100.0
"#;
        let sets = vec![
            "live_delay_ms=250.0".to_string(), // float overrides the base
            "quickview_max_width=640".to_string(), // int into a u32 field
        ];
        let sc = parse_scenario(text, &sets).expect("parse with --set");
        assert_eq!(sc.tuning.live_delay_ms, 250.0);
        assert_eq!(sc.tuning.quickview_max_width, 640);
        // Unspecified fields keep their Tuning defaults.
        assert_eq!(sc.tuning.quickview_max_height, crate::Tuning::default().quickview_max_height);
        // A bare scenario (no sets) still parses and uses defaults.
        let plain = parse_scenario(text, &[]).expect("parse no sets");
        assert_eq!(plain.tuning.live_delay_ms, 100.0);
    }

    /// Compare renders a B−A delta per metric and stays verdict-free.
    #[test]
    fn compare_shows_signed_deltas_without_verdicts() {
        let a = vec![summary("s", true, 250.0, 0), summary("s", true, 250.0, 0)];
        let b = vec![summary("s", true, 300.0, 2), summary("s", true, 300.0, 2)];
        let md = compare_markdown(&a, &b, "before", "after");
        // selected/spawn_to_served p50: 250 → 300, delta +50.
        assert!(md.contains("| 250 | 300 | +50 |"), "latency delta:\n{md}");
        // late_frames 0 → 2, delta +2.00.
        assert!(md.contains("+2.00"), "counter delta");
        assert!(!md.to_lowercase().contains("regression"), "no verdicts");
        assert!(!md.to_lowercase().contains("better"), "no verdicts");
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
        let summary = run(&scenario, &out, &[]).expect("runner completes");
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
