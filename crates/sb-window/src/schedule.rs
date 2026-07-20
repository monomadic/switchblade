//! The redraw-cadence policy, factored out of the winit event loop so it
//! is unit-testable and so the Tier-A benchmark runner (in `sb-app`) can
//! drive a headless `frame()` loop on the *exact* same schedule the real
//! window uses. `about_to_wait` and the runner both call [`next_frame`];
//! neither owns a private copy of the rules, so they cannot drift.
//!
//! The policy is pure — `Instant`, `Duration`, and three bits of window
//! state in, a [`NextFrame`] verdict out. No winit, no wgpu, no I/O.

use std::time::{Duration, Instant};

/// Redraw cadence while idle — keeps config hot-reload and stdin ingest
/// ticking without burning CPU on a static grid.
pub const IDLE_TICK: Duration = Duration::from_millis(100);

/// Floor on the continuous-redraw interval. Pacing normally comes from
/// the blocking vsync present inside `render()`, but any path where the
/// present is skipped (occluded surface, lost surface) would otherwise
/// let the Poll loop free-run `frame()` and peg a core. 4ms caps the
/// runaway at 250fps while staying under every real refresh interval
/// (240Hz = 4.16ms), so it never throttles a visible window.
pub const MIN_FRAME: Duration = Duration::from_millis(4);

/// When the loop should next produce a frame, independent of winit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextFrame {
    /// Redraw now. `poll` keeps the event loop hot for continuous
    /// animation (winit `ControlFlow::Poll`); when false the redraw fires
    /// but control flow is left as the previous waiter set it — the
    /// idle-deadline-reached path, which does not re-arm Poll.
    Now { poll: bool },
    /// Sleep until this instant (winit `ControlFlow::WaitUntil`), then
    /// redraw.
    At(Instant),
}

/// Decide the next wake from the last frame's `Frame.animating` /
/// `redraw_at` verdict plus live window state. The single source of
/// truth for the redraw cadence.
///
/// - **Animating and visible:** game-style continuous redraw, floored at
///   [`MIN_FRAME`]. Never continuous while occluded — an occluded window
///   never presents, so the continuous path has no vsync pacing and would
///   peg a core; occlusion falls through to the idle tick, which still
///   drains ingest / config / jobs at 10Hz.
/// - **Idle:** wake on the earliest of the [`IDLE_TICK`] housekeeping
///   beat and the one-shot `redraw_at` live-video deadline. Occluded
///   ignores the deadline (nothing presents anyway).
pub fn next_frame(
    animating: bool,
    occluded: bool,
    redraw_at: Option<Instant>,
    last_frame: Instant,
    now: Instant,
) -> NextFrame {
    if animating && !occluded {
        let next = last_frame + MIN_FRAME;
        if now >= next {
            NextFrame::Now { poll: true }
        } else {
            NextFrame::At(next)
        }
    } else {
        let mut next = last_frame + IDLE_TICK;
        if !occluded {
            if let Some(t) = redraw_at {
                next = next.min(t);
            }
        }
        if now >= next {
            NextFrame::Now { poll: false }
        } else {
            NextFrame::At(next)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animating_visible_redraws_continuously_once_min_frame_elapses() {
        let t0 = Instant::now();
        // A full MIN_FRAME after the last frame: due now, keep polling.
        assert_eq!(
            next_frame(true, false, None, t0, t0 + MIN_FRAME),
            NextFrame::Now { poll: true }
        );
        // Well past it: still due-now (never negative-sleeps).
        assert_eq!(
            next_frame(true, false, None, t0, t0 + IDLE_TICK),
            NextFrame::Now { poll: true }
        );
    }

    #[test]
    fn animating_floors_the_interval_at_min_frame() {
        let t0 = Instant::now();
        // Just after a present, before MIN_FRAME: sleep to the floor, not
        // spin — the runaway guard for a skipped vsync present.
        assert_eq!(
            next_frame(true, false, None, t0, t0 + Duration::from_millis(1)),
            NextFrame::At(t0 + MIN_FRAME)
        );
    }

    #[test]
    fn idle_waits_a_full_tick_then_redraws_without_polling() {
        let t0 = Instant::now();
        assert_eq!(
            next_frame(false, false, None, t0, t0 + Duration::from_millis(1)),
            NextFrame::At(t0 + IDLE_TICK)
        );
        // Idle-due fires a redraw but must NOT re-arm Poll.
        assert_eq!(
            next_frame(false, false, None, t0, t0 + IDLE_TICK),
            NextFrame::Now { poll: false }
        );
    }

    #[test]
    fn live_deadline_caps_the_idle_wait() {
        let t0 = Instant::now();
        let due = t0 + Duration::from_millis(33); // ~30fps frame, sooner than the tick
        assert_eq!(
            next_frame(false, false, Some(due), t0, t0 + Duration::from_millis(1)),
            NextFrame::At(due)
        );
        // A deadline later than the tick never delays the housekeeping beat.
        let far = t0 + Duration::from_millis(500);
        assert_eq!(
            next_frame(false, false, Some(far), t0, t0 + Duration::from_millis(1)),
            NextFrame::At(t0 + IDLE_TICK)
        );
    }

    #[test]
    fn occluded_never_redraws_continuously_and_ignores_the_deadline() {
        let t0 = Instant::now();
        let due = t0 + Duration::from_millis(5);
        // Animating but occluded: falls through to the idle tick, no Poll.
        assert_eq!(
            next_frame(true, true, None, t0, t0 + IDLE_TICK),
            NextFrame::Now { poll: false }
        );
        // The live deadline is ignored while occluded — full idle tick.
        assert_eq!(
            next_frame(false, true, Some(due), t0, t0 + Duration::from_millis(1)),
            NextFrame::At(t0 + IDLE_TICK)
        );
    }
}
