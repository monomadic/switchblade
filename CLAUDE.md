# Switchblade — agent notes

GPU-rendered video clip picker: "fzf for videos". Pipe paths in on stdin, fly a grid of clips, act on the selected one.

**[PLAN.md](PLAN.md) is the source of truth** for scope, milestones, and every design decision. Read the relevant section before structural changes; don't re-litigate settled decisions (torus deferral, filesystem cache over SQLite, animated thumbs as MVP v2, etc.) without the user.

## Status
- **Done:** M0–M6 (see PLAN.md §14) plus the internal quickview (M5's "later" form): `Space` or clicking the selection opens a dimmed modal playing the clip **at natural resolution** — a dedicated hires texture beside the atlas (`quickview_max_width/height`, default 1080p cap, never upscaling past the source), fed by its own decoder lane (`QvLive`, replaces the tile-size selected lane while open). Any click / Esc / Space closes; arrows browse without leaving it.
- **Media pipeline:** ffmpeg/ffprobe workers behind a **two-priority queue** (visible statics always beat anim sheets) → sidecar cache under `~/Library/Caches/switchblade/v1/objects/<fp>/` (artifacts named by recipe, e.g. `thumb_fit_640x360_q5.jpg`, `anim_3x3_640x360_q5.jpg` + `meta.json`) → decoded to RGBA **via ffmpeg** (not the image crate — keeps thumb and live-video color math identical; the image crate is header-dims only) → runtime-sized GPU atlas (`AtlasCfg`, from `thumb_width/height` + `atlas_width/height` tuning, startup-only).
- **Live playback:** two lanes (selected + hovered), each an ffmpeg child → raw RGBA → a never-evicted atlas slot; starts after `live_delay_ms` of target rest, seek-matched to the thumbnail's frame (10% duration via cached meta), killed on move.
- **Slot budget & eviction classes:** statics claim budget center-out, anims get the remainder; eviction order = out-of-zone anims → out-of-zone statics → in-zone anims; never in-zone statics or live slots.
- **Grid feel:** per-clip scale + emphasis springs (hover and keyboard ride the same morph: grid crop-fill → true-aspect cover, UV crop derives from live shape); anim sheets cycle by UV window with per-clip phase and shader crossfade (`anim_crossfade`, second UV per instance); reflow crossfade on zoom/resize; loading dots + cloud badges + background-jobs progress bar are all just extra tiles.
- **Perf:** idle render throttling — app returns `Frame.animating`; the loop drops to a 100ms tick when false (~2% core), wakes on input. `a` / `--no-anim` toggles sheet animation (generation is skipped entirely when off).
- **Emphasized tiles never show anim sheets** (tiny crop-fill frames zoom badly under the morph and live lands on different content) — static thumb until live arrives.
- **Focus pause:** losing window focus stops live lanes + sheet cycling (grid stays, still) and lets the idle throttle engage; `pause_unfocused` config (default true), `p` toggles at runtime.
- **Next:** M7 search/filter, hardware decode for live lanes (`-hwaccel videotoolbox`), quickview scrub/seek.

## Build & run
```sh
cargo run                          # demo mode: 480 fake tiles (stdin is a TTY)
fd -e mp4 -e mov . ~/Clips | cargo run    # real paths, streamed
RUST_LOG=debug cargo run           # more logging
```
Keys: `hjkl`/arrows move selection (reserved, not remappable; horizontal moves are linear — row-end wraps to the next row) · `Enter`/`o` open in mpv · `Space` internal quickview · `c` copy path · `r` reveal in Finder · `a` toggle anim · `p` toggle focus-pause · `f` fullscreen · `-`/`=`/`0` zoom · `q` quit. All non-movement keys remappable via `[keys]`/`[commands]` in `switchblade.toml` (external programs with `{path}`/`{dir}`/`{name}` templates, or internal actions — see `commands.rs` for the list). Trackpad pans without changing selection; pinch zooms; click selects; clicking the selection quickviews. iCloud placeholder files get a blue tile + cloud badge and are never read.

## Layout
- `src/main.rs` — thin entrypoint.
- `crates/sb-window` — owns the winit loop + wgpu renderer; defines the `App` trait boundary. Instanced rounded-rect tiles in `tiles.wgsl`.
- `crates/sb-app` — implements `App`: grid model, selection, motion, stdin ingest, tuning hot-reload. Headless of OS/GPU types.
- `crates/sb-media` — worker pool (priority queue), ffmpeg/ffprobe invocation, sidecar cache, fingerprinting, `LivePlayer`.

## Hard rules (distilled from PLAN.md)
- **Boundary:** no winit/wgpu types in `sb-app`. `sb-window` owns the loop and calls into the `App` trait; apps see normalized `InputEvent`s and return a plain `Frame`.
- **stdin is sacred:** streaming, newline + NUL delimited, never block the UI on a slow producer. File stats happen on the ingest thread, not the render path.
- **No text stack until M7.** Labels are window-title or bitmap-font debug quality only.
- **Filesystem cache, no SQLite** (until profiling proves otherwise — PLAN.md §8).
- **Feel constants live in `Tuning`** (`crates/sb-app/src/tuning.rs`), hot-reloaded from `./switchblade.toml` (mtime poll, 250ms). Never hardcode a motion/feel value elsewhere — add a field.
- Motion must stay frame-rate independent: use `tuning::alpha(k, dt)` for smoothing, never raw per-frame lerps.
- Keep dependencies lean; ask before adding one.

## Conventions
- rustfmt defaults, edition 2021.
- `cargo build` from repo root builds everything; the app reads `switchblade.toml` from the cwd, so run from repo root during development.
