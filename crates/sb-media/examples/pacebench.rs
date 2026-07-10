//! Live-playback pacing benchmark (PERF.md Phase 0).
//!
//! Measures `LivePlayer` frame delivery exactly as the app consumes it:
//! poll `take_frame()` every 16.67ms (a 60Hz render tick) and report
//! delivered fps, interval percentiles, and gap count. Acceptance
//! criteria for the PERF.md phases are runs of this benchmark.
//!
//! ```sh
//! cargo run --release -p sb-media --example pacebench -- <clip> <w> <h> <fps> [secs] [sw|swscale]
//! ```
//!
//! `fps` is the pacing rate handed to `LivePlayer` (what cached meta
//! would supply); codec and pix_fmt come from a real ffprobe of the
//! clip. A trailing `sw` forces the all-software chain (Phase 2
//! acceptance); `swscale` keeps hardware decode but software scaling —
//! the pre-scale_vt configuration, for isolating regressions.
//!
//! Discard each clip's FIRST run: cold file access (page cache, initial
//! disk reads) shows up as a burst of ~2×-interval gaps that vanishes
//! on the rerun. Benchmark numbers come from warm runs.
//!
//! Standard deterministic test assets (also listed in PERF.md):
//!
//! ```sh
//! ffmpeg -y -f lavfi -i "testsrc2=duration=8:size=3840x2160:rate=60" \
//!   -c:v hevc_videotoolbox -b:v 60M -pix_fmt yuv420p -tag:v hvc1 4k60.mp4
//! ffmpeg -y -f lavfi -i "testsrc2=duration=8:size=3840x2160:rate=60" \
//!   -c:v hevc_videotoolbox -profile:v main10 -b:v 30M -pix_fmt p010le -tag:v hvc1 4k60_10bit.mp4
//! # clips must be ≥8s: -stream_loop restarts on shorter loops read as gaps
//! ffmpeg -y -display_rotation 90 -i 4k60.mp4 -c copy rot90.mp4
//! ```
//!
//! **Run benchmarks strictly serially, on an idle machine, on AC power**
//! — concurrent runs contaminate the numbers (PERF.md standing rules).

use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let usage = "usage: pacebench <clip> <w> <h> <fps> [secs] [sw]";
    let path = std::path::PathBuf::from(args.next().expect(usage));
    let w: u32 = args.next().expect(usage).parse().unwrap();
    let h: u32 = args.next().expect(usage).parse().unwrap();
    let fps: f64 = args.next().expect(usage).parse().unwrap();
    let mut secs = 10.0;
    let mut force_sw = false;
    let mut force_swscale = false;
    for a in args {
        match a.as_str() {
            "sw" => force_sw = true,           // sw decode + sw scale
            "swscale" => force_swscale = true, // hw decode + sw scale (pre-scale_vt chain)
            _ => secs = a.parse().expect("secs must be a number"),
        }
    }

    let mut meta = sb_media::probe(&path).expect("ffprobe failed");
    meta.fps = Some(fps);
    if force_sw {
        meta.codec = None;
    }
    if force_sw || force_swscale {
        meta.pix_fmt = None;
    }
    println!(
        "clip {} codec={:?} pix_fmt={:?} rotation={:?} -> {w}x{h} paced @{fps}{}",
        path.display(),
        meta.codec,
        meta.pix_fmt,
        meta.rotation,
        if force_sw {
            " (forced sw)"
        } else if force_swscale {
            " (forced swscale)"
        } else {
            ""
        },
    );

    let t_spawn = Instant::now();
    let player = sb_media::LivePlayer::spawn(&path, w, h, 0.5, Some(&meta)).expect("spawn");
    let mut deliveries: Vec<f64> = Vec::new(); // seconds since spawn
    let tick = Duration::from_secs_f64(1.0 / 60.0);
    let mut next_tick = Instant::now();
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < deadline {
        if player.take_frame().is_some() {
            deliveries.push(t_spawn.elapsed().as_secs_f64());
        }
        next_tick += tick;
        let now = Instant::now();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        } else {
            next_tick = now; // don't accrue tick debt
        }
    }

    let n = deliveries.len();
    if n < 2 {
        println!("only {n} frames delivered — stream never started?");
        std::process::exit(1);
    }
    let first = deliveries[0];
    let span = deliveries[n - 1] - first;
    let intervals: Vec<f64> = deliveries.windows(2).map(|w| w[1] - w[0]).collect();
    let mut sorted = intervals.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| sorted[((sorted.len() - 1) as f64 * p) as usize] * 1000.0;
    let expected = 1000.0 / fps;
    let long_gaps = intervals
        .iter()
        .filter(|&&i| i * 1000.0 > expected * 1.8)
        .count();
    println!(
        "first frame: {:.0}ms after spawn\n\
         delivered {} frames over {:.2}s = {:.2} fps (target {:.2})\n\
         intervals ms: p50 {:.1}  p90 {:.1}  p99 {:.1}  max {:.1} (expected {:.1})\n\
         gaps >1.8x expected: {} ({:.1}/s)",
        first * 1000.0,
        n,
        span,
        (n - 1) as f64 / span,
        fps,
        pct(0.5),
        pct(0.9),
        pct(0.99),
        sorted[sorted.len() - 1] * 1000.0,
        expected,
        long_gaps,
        long_gaps as f64 / span,
    );
}
