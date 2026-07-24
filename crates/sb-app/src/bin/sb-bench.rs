//! Tier-A benchmark runner entry point (benchmarks/HARNESS.md Phase 3).
//!
//! Subcommands:
//!   sb-bench run   <scenario.toml> [--out <dir>] [--home <dir>] [--keep-home]
//!   sb-bench bench <scenario.toml> [--reps N] [--label L] [--reports <dir>]
//!
//! `run` executes ONE scenario in this process. The cache root is a
//! process-global `OnceLock`, so hermetic isolation (a fresh temp `HOME`)
//! is set up here before any sb-media call resolves it — which is why
//! `bench` orchestrates repeats by spawning `run` children, one per rep.
//! A bare `.toml` path (no subcommand) is treated as `run` for convenience.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    // Honor RUST_LOG so diagnostic logs (e.g. `animgen` sheet-gen timing,
    // "thumb generated") surface during a bench run. Quiet by default.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("bench") => cmd_bench(&args[1..]),
        Some("compare") => cmd_compare(&args[1..]),
        Some("-h") | Some("--help") | None => {
            usage();
            ExitCode::SUCCESS
        }
        Some(a) if a.ends_with(".toml") => cmd_run(&args), // bare scenario = run
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            usage();
            ExitCode::from(2)
        }
    }
}

fn usage() {
    eprintln!("usage:");
    eprintln!(
        "  sb-bench run     <scenario.toml> [--out <dir>] [--home <dir>] [--keep-home] [--set k=v ...]"
    );
    eprintln!(
        "  sb-bench bench   <scenario.toml> [--reps N] [--label L] [--reports <dir>] [--set k=v ...]"
    );
    eprintln!("  sb-bench compare <bundleA> <bundleB> [--out <report.md>]");
    eprintln!();
    eprintln!("  --set overrides a [tuning] field (knob sweep, no rebuild). Floats need a");
    eprintln!("  decimal: --set live_delay_ms=250.0. Strings pass bare: --set grid_layout=fixed.");
}

fn cmd_run(args: &[String]) -> ExitCode {
    let mut scenario: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut home: Option<PathBuf> = None;
    let mut keep_home = false;
    let mut sets: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = it.next().map(PathBuf::from),
            "--home" => home = it.next().map(PathBuf::from),
            "--keep-home" => keep_home = true,
            "--set" => {
                if let Some(kv) = it.next() {
                    sets.push(kv.clone());
                }
            }
            _ if scenario.is_none() => scenario = Some(PathBuf::from(a)),
            other => {
                eprintln!("unexpected argument: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(scenario) = scenario else {
        eprintln!("error: no scenario file given");
        return ExitCode::from(2);
    };

    // Hermetic HOME → isolated cache root, BEFORE any sb-media call.
    let (home, ephemeral) = match home {
        Some(h) => (h, false),
        None => (
            std::env::temp_dir().join(format!("sb_bench_home_{}", std::process::id())),
            !keep_home,
        ),
    };
    if let Err(e) = std::fs::create_dir_all(&home) {
        eprintln!("error: create temp HOME {}: {e}", home.display());
        return ExitCode::FAILURE;
    }
    // SAFETY: single-threaded here — no worker threads exist until the
    // Switchblade/MediaService construction inside run(), after this.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
    }

    let out = out.unwrap_or_else(|| default_out(&scenario));
    let code = match sb_app::bench::run(&scenario, &out, &sets) {
        Ok(summary) => {
            print_summary(&summary);
            if summary.valid {
                ExitCode::SUCCESS
            } else {
                eprintln!("run INVALID: {:?}", summary.invalid_reasons);
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    };
    if ephemeral {
        let _ = std::fs::remove_dir_all(&home);
    }
    code
}

fn cmd_bench(args: &[String]) -> ExitCode {
    let mut scenario: Option<PathBuf> = None;
    let mut reps: usize = 5;
    let mut label = String::from("run");
    let mut reports: Option<PathBuf> = None;
    let mut sets: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--reps" => {
                reps = it.next().and_then(|s| s.parse().ok()).unwrap_or(reps);
            }
            "--label" => label = it.next().cloned().unwrap_or(label),
            "--reports" => reports = it.next().map(PathBuf::from),
            "--set" => {
                if let Some(kv) = it.next() {
                    sets.push(kv.clone());
                }
            }
            _ if scenario.is_none() => scenario = Some(PathBuf::from(a)),
            other => {
                eprintln!("unexpected argument: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(scenario) = scenario else {
        eprintln!("error: no scenario file given");
        return ExitCode::from(2);
    };
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: current_exe: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Default reports root: benchmarks/reports beside the scenario dir.
    let reports = reports.unwrap_or_else(|| {
        scenario
            .parent()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."))
            .join("reports")
    });

    match sb_app::bench::orchestrate(&exe, &scenario, reps, &label, &reports, &sets) {
        Ok(report) => {
            println!("report: {}", report.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_compare(args: &[String]) -> ExitCode {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = it.next().map(PathBuf::from),
            _ if dirs.len() < 2 => dirs.push(PathBuf::from(a)),
            other => {
                eprintln!("unexpected argument: {other}");
                return ExitCode::from(2);
            }
        }
    }
    if dirs.len() != 2 {
        eprintln!("error: compare needs two bundle dirs (e.g. reports/<scenario>-<label>)");
        return ExitCode::from(2);
    }
    let label = |p: &Path| -> String {
        p.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string()
    };
    let (la, lb) = (label(&dirs[0]), label(&dirs[1]));
    let a = match sb_app::bench::read_bundle(&dirs[0]) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let b = match sb_app::bench::read_bundle(&dirs[1]) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let md = sb_app::bench::compare_markdown(&a, &b, &la, &lb);
    match out {
        Some(p) => {
            if let Err(e) = std::fs::write(&p, &md) {
                eprintln!("error: write {}: {e}", p.display());
                return ExitCode::FAILURE;
            }
            println!("compare report: {}", p.display());
        }
        None => print!("{md}"),
    }
    ExitCode::SUCCESS
}

fn default_out(scenario: &Path) -> PathBuf {
    let stem = scenario.file_stem().unwrap_or_default();
    scenario
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("runs")
        .join(stem)
}

fn print_summary(s: &sb_app::bench::Summary) {
    println!("=== {} ===", s.scenario);
    println!(
        "valid={} wall={:.2}s frames={} events={}{}",
        s.valid,
        s.wall_s,
        s.frames,
        s.events,
        if s.events_dropped > 0 {
            format!(" (+{} dropped)", s.events_dropped)
        } else {
            String::new()
        }
    );
    let c = &s.counters;
    println!(
        "counters: late={} reanchors={} evictions={} thumbs_cached={} drain_budget_hits={}",
        c.late_frames, c.reanchors, c.evictions, c.thumbs_cached, c.drain_budget_hits
    );
    println!(
        "tick_ms: p50={:.2} p95={:.2} max={:.2}",
        s.tick_ms.p50, s.tick_ms.p95, s.tick_ms.max
    );
    println!(
        "sched: jobs {}/{} started (hit {}, failed {}) busy={:.1}s util={:.2} | backlog peak {:.1} MB | render_stalls={} total={:.1}ms max={:.1}ms",
        c.jobs_finished,
        c.jobs_started,
        c.jobs_hit,
        c.jobs_failed,
        c.worker_busy_us as f64 / 1e6,
        s.worker_utilisation,
        c.pending_bytes_peak as f64 / (1024.0 * 1024.0),
        c.render_stalls,
        c.render_stall_us as f64 / 1000.0,
        c.render_stall_max_us as f64 / 1000.0,
    );
    println!(
        "render stalls: meta {}× {:.1}ms (clip_meta) | thumb_path {}× {:.1}ms (cached_thumb_path) \
         | meta_wait_expired={}",
        c.render_stall_meta_count,
        c.render_stall_meta_us as f64 / 1000.0,
        c.render_stall_path_count,
        c.render_stall_path_us as f64 / 1000.0,
        // The cost side of moving that read off-thread: spawns that gave
        // up waiting for meta. `meta 0× / expired 0` is the fix landing.
        c.meta_wait_expired,
    );
    println!(
        "decode reads: n={} mean={:.2}ms max={:.1}ms | >100ms={} >1s={}",
        c.decode_reads,
        if c.decode_reads > 0 {
            c.decode_read_us as f64 / c.decode_reads as f64 / 1000.0
        } else {
            0.0
        },
        c.decode_read_max_us as f64 / 1000.0,
        c.decode_read_over_100ms,
        c.decode_read_over_1s,
    );
    println!(
        "queue wait: thumb n={} mean={:.0}ms max={:.0}ms | gen n={} mean={:.0}ms max={:.0}ms | thumb_requests={}",
        c.queue_wait_thumb_count,
        if c.queue_wait_thumb_count > 0 {
            c.queue_wait_thumb_us as f64 / c.queue_wait_thumb_count as f64 / 1000.0
        } else {
            0.0
        },
        c.queue_wait_thumb_max_us as f64 / 1000.0,
        c.queue_wait_gen_count,
        if c.queue_wait_gen_count > 0 {
            c.queue_wait_gen_us as f64 / c.queue_wait_gen_count as f64 / 1000.0
        } else {
            0.0
        },
        c.queue_wait_gen_max_us as f64 / 1000.0,
        c.thumb_requests,
    );
    println!(
        "atlas: visible_tiles_max={} vs {} slots{} | full_drops={} | evictions={} | thumbs_cached={}",
        c.visible_tiles_max,
        s.atlas_slots,
        if c.visible_tiles_max > s.atlas_slots { "  <-- OVER CAPACITY" } else { "" },
        c.atlas_full_drops,
        c.evictions,
        c.thumbs_cached,
    );
    for l in &s.latencies {
        println!(
            "latency {:>8}/{:<20} n={:<3} p50={:.0}ms p95={:.0}ms max={:.0}ms",
            l.lane, l.metric, l.count, l.p50_ms, l.p95_ms, l.max_ms
        );
    }
    for cr in &s.conditions {
        match cr.at_s {
            Some(t) => println!("cond {:<18} met @ {:.2}s", cr.cond, t),
            None => println!("cond {:<18} NOT met", cr.cond),
        }
    }
}
