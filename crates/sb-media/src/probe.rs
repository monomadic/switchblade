//! Benchmark instrumentation shared across the app and media layers
//! (benchmarks/design/phase-0-contracts.md §0.1). Two record types:
//!
//! - **Counters** — monotonic lifetime tallies, always compiled in and
//!   always cheap (relaxed atomic adds). The runner samples them each
//!   tick to build curves (thumbs-on-disk over time) and reads a final
//!   snapshot for the summary.
//! - **Events** — timestamped, identity-carrying, recorded ONLY while a
//!   run has armed recording (`record_events`). Emission is a no-op
//!   otherwise: one relaxed load, and the `Event` (which clones an
//!   `Arc<str>`) is never even constructed, because `emit` takes a
//!   closure. This is what keeps tracing free in normal runs.
//!
//! Latency is always derived from a matched pair of events across
//! layers — an app-side action and a media-side or app-side completion —
//! never a single pre-aggregated number. Every live-lane event carries a
//! `lane_gen` incarnation id so a frame served by an obsolete lane can
//! never be credited to the current action.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A bounded event buffer never grows without limit — a runaway run (a
/// decoder re-anchoring every frame for a minute) drops the overflow and
/// records how many, rather than eating memory. Bench runs are seconds
/// to a minute; 100k events is far above any real run's yield.
const EVENT_CAP: usize = 100_000;

/// Which live decoder lane an event belongs to. `None` is the lane-less
/// context: worker-pool job events, which belong to a queue *tier*
/// instead (see [`Tier`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Selected,
    Warm,
    Hover,
    None,
}

impl Lane {
    fn name(self) -> &'static str {
        match self {
            Lane::Selected => "selected",
            Lane::Warm => "warm",
            Lane::Hover => "hover",
            Lane::None => "none",
        }
    }
}

/// Which worker-pool queue tier a job event belongs to — the strict
/// priority ladder in `Queues` (visible thumb → reprobe → chapters →
/// on-demand sheet → background gen sweep). Job events carry this so a
/// run can be decomposed per tier: how long each class of job occupies a
/// worker, and which tier is holding the pool when a lower one starves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Thumb,
    Reprobe,
    Chapters,
    Anim,
    Gen,
}

impl Tier {
    fn name(self) -> &'static str {
        match self {
            Tier::Thumb => "thumb",
            Tier::Reprobe => "reprobe",
            Tier::Chapters => "chapters",
            Tier::Anim => "anim",
            Tier::Gen => "gen",
        }
    }
}

/// How a worker job ended. `Hit` never touched the source file (the
/// artifact was already on disk) — separating it from `Made` is what
/// makes a slow-disk run readable: only `Made` jobs pay source I/O, so a
/// tier's true per-file cost is the `Made` distribution, and a rising
/// `Hit` share means the sweep is re-walking work it already did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Hit,
    Made,
    Failed,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Outcome::Hit => "hit",
            Outcome::Made => "made",
            Outcome::Failed => "failed",
        }
    }
}

/// A point-in-time read of the media queues — the scheduler's own state,
/// which no latency metric exposes. Sampled by the bench runner every
/// tick into a curve: tier depths show *where* work is piling up, and
/// `inflight`/`gen_running` show whether the pool is saturated or parked
/// (a deep `gen` queue with zero `gen_running` is the live/anim throttle
/// holding the sweep, not a stall).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueueDepths {
    pub thumbs: usize,
    pub reprobes: usize,
    pub chapters: usize,
    pub anims: usize,
    /// The background gen sweep's backlog (`gen` is a reserved keyword).
    pub gen_sweep: usize,
    /// Jobs a worker is executing right now (all tiers).
    pub inflight: usize,
    /// Gen-sweep jobs running — the counter `gen_live_cap` gates.
    pub gen_running: usize,
    /// Merged-request debt: results owed to callers that joined an
    /// in-flight generation instead of duplicating it.
    pub owed: usize,
    /// The workers' view of "a stream is being watched" (the gen throttle).
    pub live: bool,
}

/// What happened. Start/end pairs (see the module docs) are matched by
/// `(clip, lane_gen)`; the runner computes latencies from them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A decoder spawn was requested (app).
    DecodeSpawn,
    /// The decoder queued its first frame (media).
    DecodeReady,
    /// The app served a lane's first frame into the hires texture / an
    /// upload (app).
    FrameServed,
    /// A warm lane was promoted to the selected lane (app).
    Promotion,
    /// The pacing schedule re-anchored — a frame came due late and the
    /// player reset its wall-clock anchor instead of accruing debt
    /// (media). Invisible to the app by design, hence a media event.
    Reanchor,
    /// A worker popped a request and began work on it (media). Carries
    /// the tier; `lane_gen` is a monotonic job id, not a lane
    /// incarnation.
    JobStart,
    /// That job finished (media): same job id, plus `ms` (wall time the
    /// job occupied its worker) and `outcome`. The pair is what exposes
    /// scheduler behaviour a latency percentile hides — a 40s gen job on
    /// a cold external drive pins one of three workers for 40s, and only
    /// the occupancy view shows the pool going 3-wide on the sweep while
    /// a visible thumb waits behind it.
    JobEnd,
}

impl EventKind {
    fn name(self) -> &'static str {
        match self {
            EventKind::DecodeSpawn => "decode_spawn",
            EventKind::DecodeReady => "decode_ready",
            EventKind::FrameServed => "frame_served",
            EventKind::Promotion => "promotion",
            EventKind::Reanchor => "reanchor",
            EventKind::JobStart => "job_start",
            EventKind::JobEnd => "job_end",
        }
    }
}

/// A single recorded event. `at` is a raw `Instant`; the runner rebases
/// it to seconds-since-run-start at drain time (see [`Probe::drain`]).
#[derive(Debug, Clone)]
pub struct Event {
    pub at: Instant,
    pub kind: EventKind,
    pub lane: Lane,
    pub lane_gen: u64,
    /// The clip's path, stable across the index churn that shuffle and
    /// the D-swap inflict on live indices.
    pub clip: Option<Arc<str>>,
    /// Content-relative playback position for events where "where in the
    /// clip" matters (FrameServed: the served frame's pts; Promotion: the
    /// stream's position at handoff). Lets a scenario's event stream
    /// expose pts regressions — e.g. a re-opened clip restarting at the
    /// thumb anchor behind where its preview had played to.
    pub pts: Option<f64>,
    /// Queue tier, on job events only.
    pub tier: Option<Tier>,
    /// How a job ended (`JobEnd` only).
    pub outcome: Option<Outcome>,
    /// Wall time the job occupied its worker (`JobEnd` only).
    pub ms: Option<f64>,
}

impl Event {
    /// A lane event stamped `at`. Job-only fields stay empty — use
    /// [`Probe::mark_job`] for those.
    fn lane(at: Instant, kind: EventKind, lane: Lane, lane_gen: u64, clip: Option<Arc<str>>) -> Self {
        Event {
            at,
            kind,
            lane,
            lane_gen,
            clip,
            pts: None,
            tier: None,
            outcome: None,
            ms: None,
        }
    }
}

/// Monotonic lifetime counters. Cheap enough to update in every run.
///
/// Three groups: playback pacing, the worker pool, and main-thread phase
/// attribution. The last two were added for the slow-disk/big-library
/// investigation (docs/perf-reviews/03-slow-disk-scheduler.md) — a
/// latency percentile says an action was slow, these say which subsystem
/// spent the time.
#[derive(Default)]
pub struct Counters {
    pub frames: AtomicU64,
    pub late_frames: AtomicU64,
    pub reanchors: AtomicU64,
    pub drain_budget_hits: AtomicU64,
    pub evictions: AtomicU64,
    /// Clips whose thumbnail is known cached (gauge-like, monotonic over
    /// a run) — the runner samples this per tick for the fill curve.
    pub thumbs_cached: AtomicU64,

    // ── worker pool ────────────────────────────────────────────────
    pub jobs_started: AtomicU64,
    pub jobs_finished: AtomicU64,
    /// Jobs that found their artifact already on disk (no source I/O).
    pub jobs_hit: AtomicU64,
    pub jobs_failed: AtomicU64,
    /// Summed wall time workers spent inside jobs, in µs. Against the
    /// run's wall time × `WORKERS` this gives pool *utilisation* — the
    /// number that separates "the sweep is saturating three workers" from
    /// "three workers are parked on the throttle".
    pub worker_busy_us: AtomicU64,

    // ── result backlog (memory canary) ─────────────────────────────
    /// Results sent but not yet drained by the app. The channel is
    /// unbounded and `Ready`/`AnimReady` carry decoded RGBA, so a
    /// producer outrunning the per-frame upload budget shows up here
    /// first — and in `pending_bytes_peak` as the real memory number.
    pub pending_results: AtomicU64,
    pub pending_bytes: AtomicU64,
    pub pending_bytes_peak: AtomicU64,

    // ── ingest ─────────────────────────────────────────────────────
    /// Paths the ingest thread considered, admitted to the grid, and
    /// rejected (gatekeeper or missing file).
    pub ingest_seen: AtomicU64,
    pub ingest_admitted: AtomicU64,
    pub ingest_rejected: AtomicU64,
    /// Time the ingest thread spent in `stat` + the gatekeeper's 16-byte
    /// header read, in µs. On a slow disk this is the dominant cost of
    /// getting a library on screen at all, and it is invisible in every
    /// other metric.
    pub ingest_io_us: AtomicU64,

    // ── main-thread phase attribution ──────────────────────────────
    /// Nanoseconds the render thread spent per phase, accumulated. The
    /// runner samples deltas per tick, so a stalled frame can be blamed
    /// on a specific phase instead of the whole `frame()` blob.
    pub ns_drain_media: AtomicU64,
    pub ns_ingest_install: AtomicU64,
    pub ns_live: AtomicU64,

    // ── render-thread blocking fs ──────────────────────────────────
    /// Blocking filesystem calls performed ON the render thread — today
    /// `clip_meta` (source stat + meta.json read at live-spawn time) and
    /// `cached_thumb_path` (source stat: drag ghost, handoff dump). The
    /// phase counters above blame a phase; these name the exact op class,
    /// and on a network volume each call is a synchronous round-trip.
    pub render_stalls: AtomicU64,
    pub render_stall_us: AtomicU64,
    pub render_stall_max_us: AtomicU64,
}

/// A point-in-time read of the counters (serializable for the summary).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CounterSnapshot {
    pub frames: u64,
    pub late_frames: u64,
    pub reanchors: u64,
    pub drain_budget_hits: u64,
    pub evictions: u64,
    pub thumbs_cached: u64,
    #[serde(default)]
    pub jobs_started: u64,
    #[serde(default)]
    pub jobs_finished: u64,
    #[serde(default)]
    pub jobs_hit: u64,
    #[serde(default)]
    pub jobs_failed: u64,
    #[serde(default)]
    pub worker_busy_us: u64,
    #[serde(default)]
    pub pending_results: u64,
    #[serde(default)]
    pub pending_bytes: u64,
    #[serde(default)]
    pub pending_bytes_peak: u64,
    #[serde(default)]
    pub ingest_seen: u64,
    #[serde(default)]
    pub ingest_admitted: u64,
    #[serde(default)]
    pub ingest_rejected: u64,
    #[serde(default)]
    pub ingest_io_us: u64,
    #[serde(default)]
    pub ns_drain_media: u64,
    #[serde(default)]
    pub ns_ingest_install: u64,
    #[serde(default)]
    pub ns_live: u64,
    #[serde(default)]
    pub render_stalls: u64,
    #[serde(default)]
    pub render_stall_us: u64,
    #[serde(default)]
    pub render_stall_max_us: u64,
}

/// A drained event, rebased to seconds since the run anchor.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RelEvent {
    /// Seconds since the run's anchor instant.
    pub t: f64,
    pub kind: &'static str,
    pub lane: &'static str,
    pub lane_gen: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip: Option<String>,
    /// Content-relative playback position, when the event carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pts: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<f64>,
}

/// The shared instrumentation handle. One is created per app and cloned
/// into every live decoder lane (via [`crate::SeekablePlayer::attach_probe`]);
/// counters accumulate always, events only while recording.
pub struct Probe {
    pub counters: Counters,
    recording: AtomicBool,
    events: Mutex<Vec<Event>>,
    dropped: AtomicU64,
    /// Monotonic worker-job id, minted per popped request. Job events
    /// carry it in `lane_gen`, so a `JobStart`/`JobEnd` pair matches
    /// exactly even when several workers hold the same clip.
    next_job: AtomicU64,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            counters: Counters::default(),
            recording: AtomicBool::new(false),
            events: Mutex::new(Vec::new()),
            dropped: AtomicU64::new(0),
            next_job: AtomicU64::new(1),
        }
    }
}

impl Probe {
    /// A fresh handle. Counters live; event recording starts off.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Arm event recording (a benchmark run). Clears any prior buffer and
    /// the dropped count. Counters are untouched — read a baseline
    /// snapshot separately if you want deltas.
    pub fn record_events(&self) {
        self.events.lock().unwrap().clear();
        self.dropped.store(0, Ordering::Relaxed);
        self.recording.store(true, Ordering::Relaxed);
    }

    /// True while events are being recorded.
    pub fn recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }

    /// Record an event. Free when not recording: the closure (which
    /// clones the clip `Arc<str>`) is never called. Over the cap, the
    /// event is dropped and counted rather than growing the buffer.
    pub fn emit(&self, make: impl FnOnce() -> Event) {
        if !self.recording.load(Ordering::Relaxed) {
            return;
        }
        let mut buf = self.events.lock().unwrap();
        if buf.len() < EVENT_CAP {
            buf.push(make());
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Emit an app-thread event stamped now. Convenience over [`emit`]
    /// so callers never name `Event`; free when not recording (the clip
    /// `Arc` is only cloned inside the closure).
    ///
    /// [`emit`]: Probe::emit
    pub fn mark(&self, kind: EventKind, lane: Lane, lane_gen: u64, clip: &Arc<str>) {
        self.emit(|| Event::lane(Instant::now(), kind, lane, lane_gen, Some(clip.clone())));
    }

    /// Mint the next worker-job id. Always cheap (one relaxed add) — the
    /// worker needs an id whether or not events are armed, because the
    /// counters (`jobs_started`, `worker_busy_us`) tally in every run.
    pub fn next_job_id(&self) -> u64 {
        self.next_job.fetch_add(1, Ordering::Relaxed)
    }

    /// A worker-pool job event. `ms`/`outcome` are `JobEnd`-only; `clip`
    /// is the source path the job is about.
    pub fn mark_job(
        &self,
        kind: EventKind,
        tier: Tier,
        job: u64,
        clip: &std::path::Path,
        outcome: Option<Outcome>,
        ms: Option<f64>,
    ) {
        self.emit(|| Event {
            at: Instant::now(),
            kind,
            lane: Lane::None,
            lane_gen: job,
            clip: Some(Arc::from(clip.to_string_lossy().as_ref())),
            pts: None,
            tier: Some(tier),
            outcome,
            ms,
        });
    }

    /// A result was handed to the app's channel: track the undrained
    /// backlog and its bytes (the memory canary — see `pending_bytes`).
    pub fn result_queued(&self, bytes: u64) {
        let c = &self.counters;
        c.pending_results.fetch_add(1, Ordering::Relaxed);
        let now = c.pending_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        c.pending_bytes_peak.fetch_max(now, Ordering::Relaxed);
    }

    /// The app drained one result of `bytes`.
    pub fn result_drained(&self, bytes: u64) {
        let c = &self.counters;
        c.pending_results
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .ok();
        c.pending_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(bytes))
            })
            .ok();
    }

    /// [`mark`] with a content-relative playback position attached —
    /// FrameServed / Promotion events, where "where in the clip" is the
    /// measurement (handoff jumps are pts regressions).
    ///
    /// [`mark`]: Probe::mark
    pub fn mark_pts(&self, kind: EventKind, lane: Lane, lane_gen: u64, clip: &Arc<str>, pts: f64) {
        self.emit(|| Event {
            pts: Some(pts),
            ..Event::lane(Instant::now(), kind, lane, lane_gen, Some(clip.clone()))
        });
    }

    /// A snapshot of the counters right now.
    pub fn snapshot(&self) -> CounterSnapshot {
        let c = &self.counters;
        CounterSnapshot {
            frames: c.frames.load(Ordering::Relaxed),
            late_frames: c.late_frames.load(Ordering::Relaxed),
            reanchors: c.reanchors.load(Ordering::Relaxed),
            drain_budget_hits: c.drain_budget_hits.load(Ordering::Relaxed),
            evictions: c.evictions.load(Ordering::Relaxed),
            thumbs_cached: c.thumbs_cached.load(Ordering::Relaxed),
            jobs_started: c.jobs_started.load(Ordering::Relaxed),
            jobs_finished: c.jobs_finished.load(Ordering::Relaxed),
            jobs_hit: c.jobs_hit.load(Ordering::Relaxed),
            jobs_failed: c.jobs_failed.load(Ordering::Relaxed),
            worker_busy_us: c.worker_busy_us.load(Ordering::Relaxed),
            pending_results: c.pending_results.load(Ordering::Relaxed),
            pending_bytes: c.pending_bytes.load(Ordering::Relaxed),
            pending_bytes_peak: c.pending_bytes_peak.load(Ordering::Relaxed),
            ingest_seen: c.ingest_seen.load(Ordering::Relaxed),
            ingest_admitted: c.ingest_admitted.load(Ordering::Relaxed),
            ingest_rejected: c.ingest_rejected.load(Ordering::Relaxed),
            ingest_io_us: c.ingest_io_us.load(Ordering::Relaxed),
            ns_drain_media: c.ns_drain_media.load(Ordering::Relaxed),
            ns_ingest_install: c.ns_ingest_install.load(Ordering::Relaxed),
            ns_live: c.ns_live.load(Ordering::Relaxed),
            render_stalls: c.render_stalls.load(Ordering::Relaxed),
            render_stall_us: c.render_stall_us.load(Ordering::Relaxed),
            render_stall_max_us: c.render_stall_max_us.load(Ordering::Relaxed),
        }
    }

    /// Account one blocking filesystem operation performed on the render
    /// thread (see `Counters::render_stalls`). Callers time the op and
    /// hand the duration in; always live, like every counter.
    pub fn render_stall(&self, took: std::time::Duration) {
        let us = took.as_micros() as u64;
        self.counters.render_stalls.fetch_add(1, Ordering::Relaxed);
        self.counters.render_stall_us.fetch_add(us, Ordering::Relaxed);
        self.counters
            .render_stall_max_us
            .fetch_max(us, Ordering::Relaxed);
    }

    /// Stop recording and return the buffered events rebased to seconds
    /// since `anchor`, plus how many events overflowed the cap.
    pub fn drain(&self, anchor: Instant) -> (Vec<RelEvent>, u64) {
        self.recording.store(false, Ordering::Relaxed);
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        let evs = std::mem::take(&mut *self.events.lock().unwrap());
        let rel = evs
            .into_iter()
            .map(|e| RelEvent {
                t: e.at.saturating_duration_since(anchor).as_secs_f64(),
                kind: e.kind.name(),
                lane: e.lane.name(),
                lane_gen: e.lane_gen,
                clip: e.clip.map(|c| c.to_string()),
                pts: e.pts,
                tier: e.tier.map(Tier::name),
                outcome: e.outcome.map(Outcome::name),
                ms: e.ms,
            })
            .collect();
        (rel, dropped)
    }
}

/// The lane context a live decoder carries so its media-thread events
/// (re-anchors, first-frame-ready) can be tagged with identity. Attached
/// to a [`crate::SeekablePlayer`] right after spawn.
#[derive(Clone)]
pub struct LaneProbe {
    pub sink: Arc<Probe>,
    pub lane: Lane,
    pub generation: u64,
    pub clip: Arc<str>,
}

impl LaneProbe {
    /// Bump a counter + emit an identity-carrying event, from the media
    /// thread. `count` is the counter to increment (or none).
    pub fn mark(&self, at: Instant, kind: EventKind) {
        self.sink
            .emit(|| Event::lane(at, kind, self.lane, self.generation, Some(self.clip.clone())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ev(at: Instant, kind: EventKind, generation: u64) -> Event {
        Event::lane(
            at,
            kind,
            Lane::Selected,
            generation,
            Some(Arc::from("/clips/a.mp4")),
        )
    }

    #[test]
    fn emit_is_a_noop_until_recording_is_armed() {
        let p = Probe::new();
        let t0 = Instant::now();
        // Not recording: dropped stays zero, nothing buffered.
        p.emit(|| ev(t0, EventKind::DecodeSpawn, 1));
        let (evs, dropped) = p.drain(t0);
        assert!(evs.is_empty());
        assert_eq!(dropped, 0);

        p.record_events();
        p.emit(|| ev(t0 + Duration::from_millis(10), EventKind::DecodeSpawn, 1));
        p.emit(|| ev(t0 + Duration::from_millis(40), EventKind::FrameServed, 1));
        let (evs, _) = p.drain(t0);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].kind, "decode_spawn");
        assert!((evs[0].t - 0.010).abs() < 1e-6);
        assert_eq!(evs[1].kind, "frame_served");
        assert_eq!(evs[1].clip.as_deref(), Some("/clips/a.mp4"));
        // Drain disarms recording.
        assert!(!p.recording());
    }

    #[test]
    fn counters_accumulate_and_snapshot() {
        let p = Probe::new();
        p.counters.frames.fetch_add(3, Ordering::Relaxed);
        p.counters.late_frames.fetch_add(1, Ordering::Relaxed);
        p.counters.thumbs_cached.fetch_add(30, Ordering::Relaxed);
        let s = p.snapshot();
        assert_eq!(s.frames, 3);
        assert_eq!(s.late_frames, 1);
        assert_eq!(s.thumbs_cached, 30);
    }

    #[test]
    fn job_events_pair_by_job_id_and_carry_tier() {
        let p = Probe::new();
        p.record_events();
        let t0 = Instant::now();
        // Two workers holding the SAME clip in different tiers: only the
        // job id can pair start with end.
        let a = p.next_job_id();
        let b = p.next_job_id();
        assert_ne!(a, b);
        let clip = std::path::Path::new("/clips/a.mp4");
        p.mark_job(EventKind::JobStart, Tier::Gen, a, clip, None, None);
        p.mark_job(EventKind::JobStart, Tier::Anim, b, clip, None, None);
        p.mark_job(
            EventKind::JobEnd,
            Tier::Anim,
            b,
            clip,
            Some(Outcome::Made),
            Some(2800.0),
        );
        let (evs, _) = p.drain(t0);
        let end = evs.iter().find(|e| e.kind == "job_end").unwrap();
        assert_eq!(end.lane_gen, b);
        assert_eq!(end.tier, Some("anim"));
        assert_eq!(end.outcome, Some("made"));
        assert_eq!(end.ms, Some(2800.0));
        // The gen job never ended: an unpaired start is exactly how a
        // worker pinned by a slow-disk read shows up.
        assert_eq!(evs.iter().filter(|e| e.kind == "job_start").count(), 2);
    }

    #[test]
    fn pending_bytes_tracks_the_backlog_and_remembers_its_peak() {
        let p = Probe::new();
        p.result_queued(900_000);
        p.result_queued(900_000);
        p.result_drained(900_000);
        let s = p.snapshot();
        assert_eq!(s.pending_results, 1);
        assert_eq!(s.pending_bytes, 900_000);
        // The peak survives the drain — the number that matters for a
        // memory canary is the high-water mark, not the current depth.
        assert_eq!(s.pending_bytes_peak, 1_800_000);
    }

    #[test]
    fn lane_gen_distinguishes_incarnations() {
        // A late frame from gen 1 (obsolete lane) and the current gen 2
        // must be tellable apart so the stale one isn't credited to the
        // live action.
        let p = Probe::new();
        p.record_events();
        let t0 = Instant::now();
        p.emit(|| ev(t0, EventKind::FrameServed, 1));
        p.emit(|| ev(t0, EventKind::FrameServed, 2));
        let (evs, _) = p.drain(t0);
        assert_eq!(evs[0].lane_gen, 1);
        assert_eq!(evs[1].lane_gen, 2);
    }
}
