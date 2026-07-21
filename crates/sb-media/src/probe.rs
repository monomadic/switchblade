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

/// Which live decoder lane an event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Selected,
    Warm,
    Hover,
}

impl Lane {
    fn name(self) -> &'static str {
        match self {
            Lane::Selected => "selected",
            Lane::Warm => "warm",
            Lane::Hover => "hover",
        }
    }
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
}

impl EventKind {
    fn name(self) -> &'static str {
        match self {
            EventKind::DecodeSpawn => "decode_spawn",
            EventKind::DecodeReady => "decode_ready",
            EventKind::FrameServed => "frame_served",
            EventKind::Promotion => "promotion",
            EventKind::Reanchor => "reanchor",
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
}

/// Monotonic lifetime counters. Cheap enough to update in every run.
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
}

/// The shared instrumentation handle. One is created per app and cloned
/// into every live decoder lane (via [`crate::SeekablePlayer::attach_probe`]);
/// counters accumulate always, events only while recording.
pub struct Probe {
    pub counters: Counters,
    recording: AtomicBool,
    events: Mutex<Vec<Event>>,
    dropped: AtomicU64,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            counters: Counters::default(),
            recording: AtomicBool::new(false),
            events: Mutex::new(Vec::new()),
            dropped: AtomicU64::new(0),
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
        self.emit(|| Event {
            at: Instant::now(),
            kind,
            lane,
            lane_gen,
            clip: Some(clip.clone()),
            pts: None,
        });
    }

    /// [`mark`] with a content-relative playback position attached —
    /// FrameServed / Promotion events, where "where in the clip" is the
    /// measurement (handoff jumps are pts regressions).
    ///
    /// [`mark`]: Probe::mark
    pub fn mark_pts(&self, kind: EventKind, lane: Lane, lane_gen: u64, clip: &Arc<str>, pts: f64) {
        self.emit(|| Event {
            at: Instant::now(),
            kind,
            lane,
            lane_gen,
            clip: Some(clip.clone()),
            pts: Some(pts),
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
        }
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
        self.sink.emit(|| Event {
            at,
            kind,
            lane: self.lane,
            lane_gen: self.generation,
            clip: Some(self.clip.clone()),
            pts: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ev(at: Instant, kind: EventKind, generation: u64) -> Event {
        Event {
            at,
            kind,
            lane: Lane::Selected,
            lane_gen: generation,
            clip: Some(Arc::from("/clips/a.mp4")),
            pts: None,
        }
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
