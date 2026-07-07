# Switchblade — agent notes

GPU-rendered video clip picker: "fzf for videos". Pipe paths in on stdin, fly a grid of clips, act on the selected one.

**[PLAN.md](PLAN.md) is the source of truth** for scope, milestones, and every design decision. Read the relevant section before structural changes; don't re-litigate settled decisions (torus deferral, filesystem cache over SQLite, animated thumbs as MVP v2, etc.) without the user.

## Status
- **Done:** M0 (skeleton), M1 (fake grid + hot tuning), M2 (real file tiles, stdin order, unreadable handling, title label), M3 (static thumbnail cache: ffmpeg/ffprobe workers → sidecar cache under `~/Library/Caches/switchblade/v1/objects/` → RGBA into a fixed-slot GPU atlas with distance-based eviction; placeholder→thumb crossfade), M4 (keymap + external commands: `[keys]`/`[commands]` in switchblade.toml, `{path}`/`{dir}`/`{name}` templates, defaults in `commands.rs`), M5 in MVP form (`Space` = Quick-Look-ish looping windowed mpv), M6 first recipe (animated thumbs: 9-frame 3×3 sprite sheet per clip, `anim_3x3.jpg`, cycled by moving the UV window; static thumb stays authoritative for the emphasized tile).
- **Also done:** live in-tile playback of the selected clip (ffmpeg → raw RGBA → live atlas slot; starts after `live_delay_ms` of selection rest, killed on move); atlas slot *budget* (statics claim first, center-out, anims get the remainder; eviction classes never touch in-zone statics or the live slot — prevents the eviction storm when a large viewport wants more slots than exist); `--help`/`--version`.
- **Next:** M7 search/filter, the fuller anim recipe (~1s @ 10–20fps), hardware decode for the live tile, or the internal overlay quickview. See PLAN.md §14.
- Thumbs: fit within 640×360 keeping source aspect (`thumb_fit_640.jpg`, `-q:v 2`), frame at 10% duration, fingerprint = FNV-1a(path+size+mtime). Atlas is 12×12 slots of 640×360 (`ATLAS_*` consts in sb-window), shared by statics and anim sheets (`SlotKind`; eviction prefers dropping anims). Grid tiles crop-fill via per-instance UV rects; selected/hovered show true aspect (capped by `max_display_aspect`, pan & scan).

## Build & run
```sh
cargo run                          # demo mode: 480 fake tiles (stdin is a TTY)
fd -e mp4 -e mov . ~/Clips | cargo run    # real paths, streamed
RUST_LOG=debug cargo run           # more logging
```
Keys: `hjkl`/arrows move selection (reserved, not remappable) · `Enter`/`o` open in mpv · `Space` looping mpv preview · `c` copy path · `f` fullscreen · `-`/`=`/`0` zoom · `q` quit. All non-movement keys remappable via `[keys]`/`[commands]` in `switchblade.toml` (external programs with `{path}`/`{dir}`/`{name}` templates, or internal actions). Trackpad pans without changing selection; pinch zooms; click selects. iCloud placeholder files get a blue tile + cloud badge and are never read.

## Layout
- `src/main.rs` — thin entrypoint.
- `crates/sb-window` — owns the winit loop + wgpu renderer; defines the `App` trait boundary. Instanced rounded-rect tiles in `tiles.wgsl`.
- `crates/sb-app` — implements `App`: grid model, selection, motion, stdin ingest, tuning hot-reload. Headless of OS/GPU types.
- `crates/sb-media` — empty stub until M3.

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
