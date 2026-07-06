# Switchblade — agent notes

GPU-rendered video clip picker: "fzf for videos". Pipe paths in on stdin, fly a grid of clips, act on the selected one.

**[PLAN.md](PLAN.md) is the source of truth** for scope, milestones, and every design decision. Read the relevant section before structural changes; don't re-litigate settled decisions (torus deferral, filesystem cache over SQLite, animated thumbs as MVP v2, etc.) without the user.

## Status
- **Done:** M0 (skeleton: window, fullscreen, streaming stdin) and M1 (fake grid: placeholder tiles, hjkl/arrow selection, trackpad pan with rubber-band edges, hot tuning config). Parts of M2 rode along (streamed real paths become tiles in stdin order; window-title label; unreadable files tinted).
- **Next:** M2 polish, then M3 (static thumbnail cache via `sb-media`). See PLAN.md §14.

## Build & run
```sh
cargo run                          # demo mode: 480 fake tiles (stdin is a TTY)
fd -e mp4 -e mov . ~/Clips | cargo run    # real paths, streamed
RUST_LOG=debug cargo run           # more logging
```
Keys: `hjkl`/arrows move selection · `f` fullscreen · `Enter`/`o` open (logs until M4) · `Space` preview (logs until M4) · `q` quit. Trackpad pans without changing selection; click selects.

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
