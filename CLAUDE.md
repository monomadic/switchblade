# Switchblade — agent notes

GPU-rendered video clip picker: "fzf for videos". Pipe paths in on stdin, fly a grid of clips, act on the selected one.

**[PLAN.md](PLAN.md) is the source of truth** for scope, milestones, and every design decision. Read the relevant section before structural changes; don't re-litigate settled decisions (torus deferral, filesystem cache over SQLite, animated thumbs as MVP v2, etc.) without the user.

## Status
- **Done:** M0–M6 (see PLAN.md §14) plus the internal quickview (M5's "later" form): `Space` or clicking the selection opens a modal playing the clip **at natural resolution** (the same hires stream the selected tile plays — see below), with a **filmstrip** of neighbors along the bottom (selected centered, slides on `strip_snap_strength`, chips clickable; `strip_height`/`strip_gap`). In quickview, actions target the foreground: `h`/`l` advance clips, clicking a chip selects it, any other click / Esc / Space closes. The grid is backdrop: dimmed (`quickview_dim`) and **frosted** — the grid layer renders offscreen, walks a mip chain, and draws back blurred (`quickview_blur` = downsample levels, 0 disables; `Frame.blur` carries the split across the App boundary). **Neighbor decoders pre-warm** for all four movement destinations (±1 and ±row) in quickview AND in the grid (when `live_preview` is on): `warm` holds parked-by-backpressure players, the outgoing stream demotes to warm instead of dying, promotion serves a queued frame the same tick — so single-step selection moves show video instantly. Two ordering rules, both load-bearing: promotion MUST run before the pool is pruned to the new destinations (the selection is never its own neighbor; pruning first killed the warmed decoder and made every advance pay full cold-spawn latency, ~1s on 4K — a cold spawn's floor is probe+VideoToolbox init+first GOP, benchmarked unfixable via flags); and fresh warm spawns wait for settle + the selected stream's first frame, then run one at a time (user attention owns the CPU).
- **GPU-residency drops are retryable:** when the atlas is momentarily full, arriving thumbs/sheets are dropped back to `None` (never latched `Failed`) so they reload when visible — the disk cache (unlimited, nothing expires) makes retries cheap.
- **Media pipeline:** ffmpeg/ffprobe workers behind a **two-priority queue** (visible statics always beat anim sheets) → sidecar cache under `~/Library/Caches/switchblade/v1/objects/<fp>/` (artifacts named by recipe with the *ffmpeg* -q:v value; config `thumb_quality` is 1..10, mapped `-q:v = 12 - q` — e.g. `thumb_fit_640x360_q5.jpg`, `anim_3x3_640x360_q5.jpg` + `meta.json`) → decoded to RGBA **via ffmpeg** (not the image crate — keeps thumb and live-video color math identical; the image crate is header-dims only) → runtime-sized GPU atlas (`AtlasCfg`, from `thumb_width/height` + `atlas_width/height` tuning, startup-only).
- **Live playback:** the selected clip decodes **once** at quickview resolution (`quickview_max_*`, capped at source res) into the mipmapped hires texture — the tile samples it downscaled, quickview samples it big; one decoder, one timeline, so quickview opens instantly with no handoff/skip. Runs whenever `live_preview` is on *or* quickview is open (quickview skips the settle delay). The hovered tile gets a separate tile-size lane in a never-evicted atlas slot. All lanes seek-match the thumbnail's frame (10% duration via cached meta) and die on target move / focus loss. Background ffmpeg/ffprobe jobs run `nice -n 10` — user attention wins CPU.
- **Playback smoothness (`LivePlayer`):** frames are due-stamped into a small bounded queue (depth 3 — decode-ahead absorbs keyframe/cold-start spikes) and `take_frame` surfaces a frame only once it's due, so presentation paces on the **render clock**, not the reader's sleep. Late frames **re-anchor** the schedule instead of accruing debt (debt used to come due all at once = stutter-then-fast-forward at startup); this also makes SIGCONT after `set_parked` safe. Decode uses `-hwaccel videotoolbox` on macOS **only for h264/hevc/prores** (codec from cached meta): VP9/AV1 benchmarked *slower* through VT than plain software decode. The bounded queue is also what makes pre-warm free: an undrained player stalls ffmpeg via pipe backpressure after ~4 frames. Regression tests: `live_player_paces_frames`, `unwatched_player_stalls_then_serves_instantly`.
- **Slot budget & eviction classes:** statics claim budget center-out, anims get the remainder; eviction order = out-of-zone anims → out-of-zone statics → in-zone anims; never in-zone statics or live slots.
- **Grid feel:** per-clip scale + emphasis springs (hover and keyboard ride the same morph: grid crop-fill → true-aspect cover, UV crop derives from live shape); anim sheets cycle by UV window with per-clip phase and shader crossfade (`anim_crossfade`, second UV per instance); reflow crossfade on zoom/resize; loading dots + cloud badges + background-jobs progress bar are all just extra tiles. The jobs bar draws **above** the quickview dim so "still working" stays visible in the modal.
- **Perf:** idle render throttling — app returns `Frame.animating`; the loop drops to a 100ms tick when false (~2% core), wakes on input. `a` / `--no-anim` toggles sheet animation (generation is skipped entirely when off).
- **Emphasized tiles never show anim sheets** (tiny crop-fill frames zoom badly under the morph and live lands on different content) — static thumb until live arrives.
- **Focus pause:** losing window focus stops live lanes + sheet cycling (grid stays, still) and lets the idle throttle engage; `pause_unfocused` config (default true), `p` toggles at runtime.
- **Next:** M7 search/filter; quickview scrub/seek; **edge-peek quickview** (deferred milestone: the ±1 neighbors' *videos* peek at the screen edges and slide in on advance — pre-warmed decoders are already the feed, but it needs the single hires texture to become a small array + slide choreography).

## Build & run
```sh
cargo run                          # demo mode: 480 fake tiles (stdin is a TTY)
fd -e mp4 -e mov . ~/Clips | cargo run    # real paths, streamed
RUST_LOG=debug cargo run           # more logging
```
Keys: `hjkl`/arrows move selection (reserved, not remappable; horizontal moves are linear — row-end wraps to the next row) · `Enter`/`o` open in mpv · `Space` internal quickview · `c` copy path · `r` reveal in Finder · `a` toggle anim · `p` toggle focus-pause · `f` fullscreen · `-`/`=`/`0` zoom · `q` quit. All non-movement keys remappable via `[keys]`/`[commands]` in `switchblade.toml` (`type = "launch"` programs with `{path}`/`{dir}`/`{name}` templates, or internal actions — see `commands.rs` for the list). Trackpad pans without changing selection; pinch zooms; click selects; clicking the selection quickviews. iCloud placeholder files get a blue tile + cloud badge and are never read.

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
