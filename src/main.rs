const HELP: &str = "\
switchblade — a GPU-rendered video clip picker (fzf for videos)

USAGE:
    switchblade [OPTIONS] [PATH ...]
    fd -e mp4 . ~/Clips | switchblade

    PATH arguments (files or directories) are the input when given;
    otherwise paths stream from stdin, newline- or NUL-delimited.
    Directories recurse when `recurse = true` (the default) in
    switchblade.toml. Non-video files are skipped.

KEYS (defaults; remappable via [keys]/[commands] in ./switchblade.toml):
    hjkl / arrows   move selection
    Enter / o       open selected clip (mpv)
    Space           quickview: in-app preview (Esc closes, arrows browse)
    c               copy path
    r               reveal in Finder
    p               toggle pause-when-unfocused
    - / = / 0       zoom out / in / reset (also trackpad pinch)
    f               fullscreen
    q               quit

CONFIG:
    Feel constants, keys, and commands; hot-reloads while the app runs.
    Search order (first existing wins):
        ./switchblade.toml
        ~/.config/switchblade.toml
        ~/.config/switchblade/config.toml
    `switchblade --init` writes the annotated default config to
    ~/.config/switchblade.toml.

OPTIONS:
    --animation <none|minimal|normal>
                    how much moves (overrides the config's `animation`):
                    none = snap everything, no video; minimal = UI tweens
                    only; normal = + live video for quickview/selected/
                    hovered ('full' is a legacy alias of normal)
    --fullscreen    start fullscreen (macOS native: own Space + animation)
    --fast-fullscreen
                    start fullscreen the fast way: a borderless window
                    covering the screen — instant, no separate Space
                    (bind the `fullscreen` / `fast_fullscreen` internal
                    actions in [keys] to toggle either at runtime)
    --sort <none|newest|oldest>
                    gatekeeper ordering (overrides the config's `sort`):
                    merge arriving files into a creation-date-sorted grid
                    as they stream in, instead of appending in input
                    order — tiles glide aside as out-of-order files land
    --init          write the default config to ~/.config/switchblade.toml
    --no-config     ignore every config file: run on the internal
                    defaults, with hot-reload off (tests, triage —
                    behavior can't be steered by a stray config)
    --no-storyboards
                    never generate the storyboard sheet (the 3x3 anim
                    atlas the seekbar skimming + chapter chips sample) —
                    for perf-constrained machines and A/B testing
    --demo          fake-tile demo grid (no media needed)
    -h, --help      print this help
    -V, --version   print version

CACHE (thumbnails live under ~/Library/Caches/switchblade.noindex):
    --cleanup-cache remove entries whose source file is gone or changed,
                    plus artifacts the current config would never serve
                    (old sizes/qualities) and interrupted-write leftovers
    --clear-cache   remove the entire cache
    --reduce-cache <MB>
                    cleanup, then evict least-recently-used entries
                    until the cache fits the given size

BENCHMARK:
    --benchmark <clip>
                    measure live-playback delivery for a clip at the
                    current config's tile and quickview resolutions
                    (run on an idle machine, on AC power)
";

fn main() -> anyhow::Result<()> {
    let mut opts = sb_app::Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--animation" => {
                let level = args.next().and_then(|v| sb_app::AnimLevel::parse(&v));
                match level {
                    Some(l) => opts.animation = Some(l),
                    None => {
                        eprintln!("switchblade: --animation takes none|minimal|normal\n");
                        std::process::exit(2);
                    }
                }
            }
            eq if eq.starts_with("--animation=") => {
                match sb_app::AnimLevel::parse(&eq["--animation=".len()..]) {
                    Some(l) => opts.animation = Some(l),
                    None => {
                        eprintln!("switchblade: --animation takes none|minimal|normal\n");
                        std::process::exit(2);
                    }
                }
            }
            "--sort" => {
                let mode = args.next().and_then(|v| sb_app::SortMode::parse(&v));
                match mode {
                    Some(m) => opts.sort = Some(m),
                    None => {
                        eprintln!("switchblade: --sort takes none|newest|oldest\n");
                        std::process::exit(2);
                    }
                }
            }
            eq if eq.starts_with("--sort=") => {
                match sb_app::SortMode::parse(&eq["--sort=".len()..]) {
                    Some(m) => opts.sort = Some(m),
                    None => {
                        eprintln!("switchblade: --sort takes none|newest|oldest\n");
                        std::process::exit(2);
                    }
                }
            }
            "--fullscreen" => opts.fullscreen = Some(false),
            "--fast-fullscreen" => opts.fullscreen = Some(true),
            "--init" => return init_config(),
            "--no-config" => opts.no_config = true,
            "--no-storyboards" => opts.no_storyboards = true,
            "--demo" => opts.demo = true,
            "--clear-cache" => return clear_cache(),
            "--cleanup-cache" => return cleanup_cache(),
            "--reduce-cache" => {
                let mb = args.next().and_then(|v| v.parse::<u64>().ok());
                let Some(mb) = mb else {
                    eprintln!("switchblade: --reduce-cache takes a size in MB\n");
                    std::process::exit(2);
                };
                return reduce_cache(mb);
            }
            "--benchmark" => {
                let Some(clip) = args.next() else {
                    eprintln!("switchblade: --benchmark takes a clip path\n");
                    std::process::exit(2);
                };
                return benchmark(clip.into());
            }
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("switchblade {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            flag if flag.starts_with('-') => {
                eprintln!("switchblade: unknown option '{flag}'\n");
                eprint!("{HELP}");
                std::process::exit(2);
            }
            path => opts.inputs.push(path.into()),
        }
    }
    // Nothing to show: no path arguments, nothing piped in.
    if opts.inputs.is_empty() && !opts.demo && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        print!("{HELP}");
        std::process::exit(2);
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    sb_window::run(sb_app::Switchblade::with_options(opts))
}

/// `--init`: write the annotated example config (embedded at build time)
/// to ~/.config/switchblade.toml. Refuses to overwrite an existing file.
fn init_config() -> anyhow::Result<()> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")));
    let Some(base) = base else {
        eprintln!("switchblade: cannot resolve a config directory (no HOME)");
        std::process::exit(1);
    };
    let dst = base.join("switchblade.toml");
    if dst.exists() {
        eprintln!(
            "switchblade: {} already exists — not overwriting",
            dst.display()
        );
        std::process::exit(1);
    }
    std::fs::create_dir_all(&base)?;
    std::fs::write(&dst, include_str!("../switchblade.toml"))?;
    println!("wrote {}", dst.display());
    Ok(())
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn clear_cache() -> anyhow::Result<()> {
    let removed = sb_media::maintenance::clear()?;
    println!(
        "cleared {} ({} files, {:.1} MB)",
        sb_media::cache_root().display(),
        removed.files,
        mb(removed.bytes)
    );
    Ok(())
}

fn cleanup_cache() -> anyhow::Result<()> {
    let recipe = sb_app::recipe_from(&sb_app::load_tuning());
    let removed = sb_media::maintenance::cleanup(&recipe);
    let left = sb_media::maintenance::usage();
    println!(
        "removed {} files ({:.1} MB); {} files ({:.1} MB) remain",
        removed.files,
        mb(removed.bytes),
        left.files,
        mb(left.bytes)
    );
    Ok(())
}

fn reduce_cache(target_mb: u64) -> anyhow::Result<()> {
    let recipe = sb_app::recipe_from(&sb_app::load_tuning());
    let removed = sb_media::maintenance::reduce(&recipe, target_mb * 1024 * 1024);
    let left = sb_media::maintenance::usage();
    println!(
        "removed {} files ({:.1} MB); {} files ({:.1} MB) remain (target {target_mb} MB)",
        removed.files,
        mb(removed.bytes),
        left.files,
        mb(left.bytes)
    );
    Ok(())
}

/// `--benchmark <clip>`: measure `SeekablePlayer` delivery exactly as the
/// app consumes it (poll take_frame at a 60Hz render tick) for the
/// lanes the current config would spawn: the tile-size lane and the
/// quickview/hires lane. When the config's chain is hardware-scaled, a
/// forced software-scale pass runs too, so users can see what the hw
/// chain buys on their machine. Same methodology as the pacebench
/// example (docs/perf-reviews/01-live-video-pipeline.md Phase 0); a short warmup pass absorbs cold file
/// access first.
fn benchmark(clip: std::path::PathBuf) -> anyhow::Result<()> {
    let tuning = sb_app::load_tuning();
    let recipe = sb_app::recipe_from(&tuning);
    let Some(meta) = sb_media::probe(&clip) else {
        eprintln!("switchblade: ffprobe failed for {}", clip.display());
        std::process::exit(1);
    };
    // Display dims: ±90° rotation swaps coded width/height.
    let (mut sw, mut sh) = (
        meta.width.unwrap_or(1920) as u32,
        meta.height.unwrap_or(1080) as u32,
    );
    if meta
        .rotation
        .is_some_and(|r| ((r / 90.0).round() as i64).rem_euclid(2) == 1)
    {
        std::mem::swap(&mut sw, &mut sh);
    }
    let fps = meta.fps.unwrap_or(30.0);
    println!(
        "{}: {}x{} {} @{fps:.2} pix_fmt={} rotation={:?}",
        clip.display(),
        sw,
        sh,
        meta.codec.as_deref().unwrap_or("?"),
        meta.pix_fmt.as_deref().unwrap_or("?"),
        meta.rotation,
    );

    // Fit the source into a box without upscaling — how the app sizes
    // both the tile lane and the quickview/hires stream.
    let fit = |bw: u32, bh: u32| {
        let s = (bw as f64 / sw as f64).min(bh as f64 / sh as f64).min(1.0);
        (
            ((sw as f64 * s) as u32).max(2),
            ((sh as f64 * s) as u32).max(2),
        )
    };
    let tile = fit(recipe.thumb_w, recipe.thumb_h);
    let quick = fit(
        tuning.quickview_max_width.clamp(320, 4096),
        tuning.quickview_max_height.clamp(180, 4096),
    );

    // Warm the page cache so the first measured pass isn't charged for
    // cold disk reads (they read as a burst of startup gaps).
    println!("\nwarming up...");
    bench_pass(&clip, tile.0, tile.1, &meta, 2.0, true);

    println!("\ntile lane {}x{}:", tile.0, tile.1);
    bench_pass(&clip, tile.0, tile.1, &meta, 8.0, false);

    println!(
        "\nquickview {}x{} (current config chain):",
        quick.0, quick.1
    );
    bench_pass(&clip, quick.0, quick.1, &meta, 8.0, false);

    // The hardware scale chain needs a VT codec AND a known 4:2:0
    // pix_fmt; blanking pix_fmt forces the software chain, which is
    // what this clip would get with an unhealed (pre-pix_fmt) cache.
    let vt_codec = matches!(
        meta.codec.as_deref(),
        Some("h264" | "hevc" | "h265" | "prores")
    );
    if vt_codec && meta.pix_fmt.is_some() {
        let mut sw_meta = meta.clone();
        sw_meta.pix_fmt = None;
        println!(
            "\nquickview {}x{} (forced software scale):",
            quick.0, quick.1
        );
        bench_pass(&clip, quick.0, quick.1, &sw_meta, 8.0, false);
    } else {
        println!(
            "\n(no hardware-scale comparison: {})",
            if vt_codec {
                "pix_fmt unknown — software chain either way"
            } else {
                "codec doesn't go through VideoToolbox"
            }
        );
    }
    println!(
        "\nconfig knobs: thumb_width/height, quickview_max_width/height, \
         thumb_quality, anim_grid (switchblade.toml, hot-reloaded)"
    );
    Ok(())
}

/// One measurement pass: spawn, poll at 60Hz for `secs`, report
/// delivery stats against the clip's own frame rate (`quiet` runs the
/// pass without reporting — the warmup).
fn bench_pass(
    clip: &std::path::Path,
    w: u32,
    h: u32,
    meta: &sb_media::Meta,
    secs: f64,
    quiet: bool,
) {
    use std::time::{Duration, Instant};
    let fps = meta.fps.unwrap_or(30.0).clamp(1.0, 240.0);
    let t_spawn = Instant::now();
    let Some(player) = sb_media::SeekablePlayer::spawn(clip, w, h, 0.5, Some(meta)) else {
        println!("  spawn failed (is ffmpeg on PATH?)");
        return;
    };
    let mut deliveries: Vec<f64> = Vec::new();
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
    if quiet {
        return;
    }
    if n < 2 {
        println!("  only {n} frames delivered — stream never started?");
        return;
    }
    let span = deliveries[n - 1] - deliveries[0];
    let mut intervals: Vec<f64> = deliveries.windows(2).map(|w| w[1] - w[0]).collect();
    intervals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| intervals[((intervals.len() - 1) as f64 * p) as usize] * 1000.0;
    let expected = 1000.0 / fps;
    let gaps = intervals
        .iter()
        .filter(|&&i| i * 1000.0 > expected * 1.8)
        .count();
    println!(
        "  first frame {:.0}ms after spawn\n  \
         {:.2} fps delivered (target {:.2}, {} frames / {:.2}s)\n  \
         intervals ms: p50 {:.1}  p90 {:.1}  p99 {:.1}  max {:.1} (expected {:.1})\n  \
         gaps >1.8x expected: {} ({:.2}/s)",
        deliveries[0] * 1000.0,
        (n - 1) as f64 / span,
        fps,
        n,
        span,
        pct(0.5),
        pct(0.9),
        pct(0.99),
        intervals[intervals.len() - 1] * 1000.0,
        expected,
        gaps,
        gaps as f64 / span,
    );
}
