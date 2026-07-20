//! Tier-A benchmark runner entry point (benchmarks/TASKS.md Phase 3).
//!
//! One scenario per process — the cache root is a process-global
//! `OnceLock`, so hermetic isolation (a fresh temp `HOME`) is set up here,
//! before any sb-media call resolves it. The orchestration of N repeats
//! across scenarios spawns this binary once per run.
//!
//! Usage:
//!   sb-bench <scenario.toml> [--out <dir>] [--home <dir>] [--keep-home]
//!
//! `--home` pins the temp HOME (default: a fresh temp dir, removed on exit
//! unless `--keep-home`). `--out` is where summary.json + events.jsonl land
//! (default: <scenario-dir>/runs/<scenario-stem>).

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut scenario: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut home: Option<PathBuf> = None;
    let mut keep_home = false;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => out = args.next().map(PathBuf::from),
            "--home" => home = args.next().map(PathBuf::from),
            "--keep-home" => keep_home = true,
            "-h" | "--help" => {
                eprintln!(
                    "usage: sb-bench <scenario.toml> [--out <dir>] [--home <dir>] [--keep-home]"
                );
                return ExitCode::SUCCESS;
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
        eprintln!("usage: sb-bench <scenario.toml> [--out <dir>] [--home <dir>] [--keep-home]");
        return ExitCode::from(2);
    };

    // Hermetic HOME → isolated cache root. Must happen before the first
    // sb-media call (which OnceLock-resolves the cache root from HOME).
    let (home, ephemeral) = match home {
        Some(h) => (h, false),
        None => {
            let pid = std::process::id();
            let h = std::env::temp_dir().join(format!("sb_bench_home_{pid}"));
            (h, !keep_home)
        }
    };
    if let Err(e) = std::fs::create_dir_all(&home) {
        eprintln!("error: create temp HOME {}: {e}", home.display());
        return ExitCode::FAILURE;
    }
    // SAFETY: single-threaded here — no worker threads spawn until the
    // first Switchblade/MediaService construction, which happens inside
    // run() after this returns.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
    }

    let out = out.unwrap_or_else(|| {
        let stem = scenario.file_stem().unwrap_or_default();
        scenario
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("runs")
            .join(stem)
    });

    let code = match sb_app::bench::run(&scenario, &out) {
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
    println!("tick_ms: p50={:.2} p95={:.2} max={:.2}", s.tick_ms.p50, s.tick_ms.p95, s.tick_ms.max);
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
