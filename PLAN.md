# SWITCHBLADE

> A GPU-rendered video clip picker.
> Feed it clips on stdin, fly the grid, pick the shot.
> Built in Rust. Minimal, fast, and clip-first.

---

## 1. What this is

Switchblade is a minimalist **video clip browser / picker**.

It is not an editor, not a DAM, not a file manager, and not a database app. Think:

```sh
fd -e mp4 -e mov . ~/Clips | switchblade
```

Like `fzf`, but for visual clip selection instead of lines of text.

**The product is the grid:** a fast, beautiful, GPU-rendered surface that lets you skim a large set of clips quickly.

---

## 2. Product shape

### Primary use
- Pipe in a list of video files.
- See them as a responsive grid.
- Move through them with keyboard or pointer.
- Preview the selected clip.
- Open/send/run actions on the selected clip.

### Primary users
- VJs picking clips mid-set.
- Personal media-library skimming.
- AI-video creators sorting generated clips.
- Anyone who wants a fast visual selector for video files.

### Non-goals
- No editing.
- No timeline.
- No trimming.
- No asset-management database.
- No folder tree.
- No mandatory import step.
- No many-live-video-wall in the MVP.

---

## 3. Design pillars

1. **Grid first.**
   The grid and its fluid feel are the first priority — it must feel rapid and alive like a game or an Apple app before the backend gets clever.
2. **Fake everything until the interaction proves itself.**
   Start with placeholder tiles. Tune motion. Only then add thumbnails, cache, preview, and optimization.
3. **Filesystem-first, stdin-first.**
   Switchblade should feel like a Unix tool: receive paths, display paths, act on paths. Treat stdin at least as respectfully as fzf does: stream paths as they arrive, never wait for EOF, never block the UI on a slow producer, accept newline and NUL delimiters, and stay usable while input trickles in.
4. **Animated thumbnails, not live decode walls.** *(post-MVP)*
   When the field gets animated, it's through cached animated thumbnails, not dozens of simultaneous video decoders. Static thumbnails ship first.
5. **Selected clip gets privilege.**
   The selected clip may get a real live preview. Everything else is cached/static/animated.
6. **Minimal chrome.**
   No heavy UI theme system. No faux desktop. No window manager vibes. Just clips on a dark field.

---

## 4. MVP interaction model

### Keyboard
- `hjkl` / arrows: move selection by one cell.
- `Enter` / `o`: open selected clip with external command.
- `Space`: preview selected clip.
- `/`: fuzzy search/filter (later).
- `Esc`: close preview/search. *(No-op until the internal overlay exists — external mpv closes itself.)*
- `q`: quit.

> **Note:** in the MVP, preview and open both spawn mpv. Give them distinct meanings from day one — e.g. preview uses `mpv --loop --profile=switchblade-preview` (looping, borderless, auto-sized) while open is a plain `mpv {path}`.

### Pointer / trackpad
- Drag/pan moves the grid.
- Pinch zooms the grid: tile size scales and columns reflow, anchored on the viewport center. Also on `-` / `=` / `0` keys.
- Hover may highlight.
- Click selects.
- Double-click or configured action opens.

### Selection vs viewport
- **Selected:** the clip actions target.
- **Viewport position:** where the grid has been panned/scrolled.
- Pointer movement must not accidentally change selection unless clicked.

---

## 5. Infinite grid / wrap policy

The infinite torus/wrapping grid is **not MVP** — and a finite grid is arguably better for a picker: stable positions build spatial memory ("that clip lives top-left"), which wrapping destroys.

Default behavior:
- finite grid,
- inertial pan,
- soft edges or normal bounds,
- stable spatial memory.

Later:

```sh
switchblade --wrap
```

If wrapping is trivial, it can exist as a non-default experimental mode. If not, it waits.

---

## 6. Media strategy

Switchblade has three media levels.

### Level 1: static thumbnails
The first real media milestone.
- Extract one representative frame.
- Cache it.
- Render it as a tile.
- The app must remain usable while thumbnails are missing/generating.
- iCloud placeholder (dataless) files are detected at ingest and shown with a cloud badge; their data is **never** read, since reading would trigger a download. (Later: a "download selected" action.)

Initial policies: first frame, N% into clip; maybe later a scene-ish frame.

### Level 2: animated thumbnails *(MVP v2 — after selected preview)*
This is the "alive grid" feature, deliberately deferred: the grid's fluid motion is the priority, and it must feel great with static tiles first.

Animated thumbnails are **cached preview strips or small sidecar previews, not live decoders per tile**.

**Cheapest first recipe:** 5–10 stills per clip, cycled slowly over a few seconds. This reuses the static-thumbnail machinery almost verbatim (N frames instead of 1, packed into one sprite sheet) and may already be enough aliveness.

Fuller target after that:
- 1 second duration,
- 10–20 fps,
- low resolution,
- looped silently,
- generated in background,
- generated for visible / nearby / recently selected files first (including in the direction of pan).

Later policies: one second from 25%, one second from 50%, several micro-clips from different parts, configurable preview recipe.

### Level 3: selected live preview
Only the **selected** clip gets real playback.

MVP version:
- spawn mpv externally.

Later internal version:
- overlay preview inside Switchblade,
- hardware decoded,
- seekable,
- loopable,
- no need to support many simultaneous live decoders.

---

## 7. Embedded animated thumbnails vs sidecar cache

Embedding animated thumbnails inside MP4/MOV files is **not** the simpler path. It sounds elegant, but it creates more problems:
- mutating original media files,
- container compatibility weirdness,
- metadata preservation risks,
- extra failure modes,
- slower batch generation,
- user trust issues,
- harder cache invalidation,
- poor portability across tools.

Preferred approach — sidecar cache:

```
video.mp4
cache/
  <fingerprint>/
    thumb.jpg
    anim.webp / anim.mp4 / strip.bin
    meta.json
```

Sidecar cache is simpler, safer, reversible, and works with read-only media. A separate future tool can generate portable preview assets, but Switchblade should not depend on embedded previews.

---

## 8. Cache design: filesystem-first

**Avoid SQLite for the MVP.** The cache is a normal directory tree under the platform cache dir:

```
~/Library/Caches/switchblade/
  v1/
    objects/
      ab/
        abcdef123456/
          meta.json
          thumb.jpg
          anim.webp
          anim.index.json
```

### Fingerprint key
Pragmatic file fingerprint for MVP:

```
absolute path + size + mtime
```

> **Tradeoff:** keying on the path means moved/renamed files lose their cache — and clip libraries get reorganized a lot. Later, optional stronger modes fix this: `size + mtime + partial hash` (one cheap 64KB read, survives moves) or full content hash. Store the source path in `meta.json` regardless, for debugging.

### Cache files

| File         | Contents |
|--------------|----------|
| `meta.json`  | duration, dimensions, codec, fps, rotation, source path snapshot |
| `thumb.jpg`  | static thumbnail |
| `anim.webp`  | animated thumbnail, *or* |
| `anim.mp4`   | tiny preview video, *or* |
| `strip.rgba` | custom frame strip |

### Why filesystem cache first
- no DB dependency,
- easy to inspect,
- easy to delete,
- easy to debug,
- works with a single binary,
- fits the "fzf for videos" model.

Also: fingerprinting requires a `stat()` per file at startup anyway, so the filesystem cache's lookup cost is nearly free on top of unavoidable work.

### When SQLite becomes worth it
SQLite is not a server dependency and can ship inside a single binary via Rust crates — the "dependency" concern is mild. Still: filesystem cache first, SQLite only after profiling proves the filesystem model hurts. Add it later only if needed for:
- huge persistent libraries,
- fuzzy metadata search,
- cross-run ranking,
- fast global cache lookup,
- large-scale eviction accounting.

---

## 9. Rendering

Use a **custom GPU surface, not a widget toolkit**.

| Concern           | Choice |
|-------------------|--------|
| Window/event loop | `winit` |
| GPU               | `wgpu` |
| Tile rendering    | custom instanced quads |
| Text              | minimal; defer fancy text stack |
| Debug tuning      | simple hot-reload config first |

### Grid rendering
- one instanced quad per visible tile,
- per-instance position, scale, opacity, texture index / UV offset,
- CPU-updated motion state initially,
- GPU compute only if needed later.

### Visual style
Minimal:
- dark background,
- clean tiles,
- subtle focused-tile emphasis,
- maybe mild glow,
- almost no UI chrome.

No big theme system in MVP. **Do** reserve a single optional fullscreen post-fx pass slot in the pipeline (one shader hook, off by default) — it costs nothing now and lets scanlines/glow/CRT flavor drop in later without restructuring.

**Juice, not chrome:** small motion details that make it feel like a game / Apple app are always in scope and cheap in this architecture — animated selection border, spring scale on select, glow pulse on the focused tile, tiles fading/scaling in as thumbnails arrive, soft-edge bounce on pan. These are per-instance shader params plus tuning values, not UI framework features.

### Text (scoping note)
"Filename labels" can quietly pull in a full text stack (shaping, atlases, fallback). Text rendering is deferred until well after the grid has proven itself — real text lands with search (M7, post-MVP), and even then only what search needs. Until then, labels are debug-quality only: an embedded bitmap font, or just the selected clip's filename in the window title. The thumbnail is the tile's identity.

---

## 10. Tuning / juice

This is critical and happens **before** real thumbnails.

Hot-reloaded config:

```toml
[tuning]
tile_width = 240
tile_height = 135
gap = 18
pan_friction = 0.88
selection_scale = 1.08
hover_scale = 1.04
snap_strength = 0.22
```

Goal:
- tweak motion without recompiling,
- keep the feel loop tight,
- avoid committing to big architecture before the grid feels right.

---

## 11. External commands

Switchblade should be useful as a **launcher/hub**.

Example config:

```toml
[keys]
enter = "open"
o = "open"
space = "preview"
r = "rename_script"
c = "copy_path"

[commands.open]
type = "external"
program = "mpv"
args = ["{path}"]

[commands.rename_script]
type = "external"
program = "~/bin/rename-media"
args = ["{path}"]

[commands.copy_path]
type = "internal"
action = "copy_path"
```

Actions target the selected clip. Batch/multiselect can come later.

---

## 12. Architecture

Three crates initially.

```
switchblade/
├── crates/
│   ├── sb-app
│   │   # CLI, stdin ingest, app state, command dispatch,
│   │   # config, orchestration
│   │
│   ├── sb-window
│   │   # winit event loop, OS/windowing integration,
│   │   # input normalization, fullscreen, HiDPI
│   │
│   └── sb-media
│       # thumbnail extraction, animated thumbnail generation,
│       # filesystem cache, media probing
│
└── src/
    # thin binary entrypoint
```

### Why sb-window is separate
Windowing and OS event details get hairy fast. Keeping them isolated helps:
- portability,
- testing app logic headlessly,
- avoiding OS-specific mess in core app code,
- swapping or mocking window/input behavior.

> **Boundary direction:** modern `winit` inverts control — the event loop owns the application via `ApplicationHandler`. So the workable shape is `sb-window` *owning the loop and calling into an app trait* that `sb-app` implements, not a windowing library that `sb-app` drives. Design the boundary that way from the start.

### Where rendering lives
Initially, rendering lives in `sb-window`, tightly coupled to wgpu surface setup. If it grows large, split into `sb-render` later. **Do not split prematurely.**

---

## 13. Dev methodology

**Rule: do not optimize the backend until the grid proves itself.**

Sequence:
1. fake grid,
2. tune motion,
3. prove it feels good,
4. add static thumbnails,
5. add cache,
6. add external open,
7. add selected preview,   ← MVP complete here
8. add animated thumbnails,
9. optimize media backend.

---

## 14. Milestones

### M0 — Skeleton
- CLI accepts stdin paths (newline and NUL-delimited), **streaming** — don't wait for EOF.
- Window opens.
- Fullscreen toggle.
- Logs ingested paths.

**Exit criteria:** `fd -e mp4 . ~/Clips | switchblade` opens a window and receives paths as they arrive.

### M1 — Fake grid
- Render placeholder tiles.
- Keyboard navigation.
- Pointer pan with inertia.
- Selection state.
- Smooth motion.
- Hot tuning config.

**Exit criteria:** the grid feels good even with fake tiles.

### M2 — Real file tiles
- Tiles appear as paths stream in; preserve stdin order; stable placement.
- Debug-quality labels only (bitmap font or window-title filename — no text stack, see §9).
- Handle missing/unreadable files gracefully.

**Exit criteria:** a real piped file list becomes a navigable visual grid, still without thumbnails.

### M3 — Static thumbnail cache
- Generate static thumbnails in background, prioritized by visibility/proximity.
- Store in filesystem cache.
- Load cached thumbnails on restart.
- Never block the render thread.

**Exit criteria:** second launch over the same files feels instant.

### M4 — External open/actions
- Open selected clip in mpv.
- Basic configurable commands.
- Copy path.
- Reveal in Finder (optional).

**Exit criteria:** Switchblade is already useful as a visual picker.

### M5 — Selected preview
- `Space` previews the selected clip.
- MVP may spawn mpv (with a distinct preview profile — loop, borderless).
- Later: internal overlay preview.

**Exit criteria:** you can skim, select, preview, and open without leaving the flow.

---

**═══ MVP complete at M5. Everything below is MVP v2. ═══**

---

### M6 — Animated thumbnails *(MVP v2)*
- First recipe: 5–10 stills per clip cycled over a few seconds (reuses static-thumb machinery, packed as a sprite sheet).
- Then: tiny animated previews (~1s, 10–20fps) generated in background.
- Prioritize selected / nearby / visible clips (and direction of pan).
- Loop animated previews in the grid.
- Configurable preview recipe.
- **GPU residency:** upload sheets for visible + nearby tiles only; LRU-evict the rest. Thousands of animated thumbs won't all fit in VRAM.

**Exit criteria:** the grid feels alive without live-decoding many full videos.

### M7 — Search/filter *(MVP v2)*
- Fuzzy filename search.
- Filter current input set.
- Keep selection sane across filters.
- (Real text rendering lands here — first time a text stack enters the codebase.)

**Exit criteria:** large clip sets become practical.

### Later
- `--wrap` infinite grid mode.
- Internal hardware-decoded preview.
- Better thumbnail frame selection.
- Multi-select.
- Batch actions.
- Metadata search.
- Optional SQLite index if the filesystem cache proves insufficient.
- Optional split into `sb-render`.
- Optional platform-specific decode backends.
- Optional post-fx flavor pass (scanlines/glow) via the reserved shader slot.

---

## 15. Open technical spikes

### Thumbnail format
Compare:
- JPEG stills,
- animated WebP,
- tiny MP4 preview clips,
- raw frame strips / **sprite sheets** ← *current favorite*.

Decision criteria: generation speed, decode speed, GPU upload cost, cache size, simplicity.

> **Leaning:** a sprite sheet (N frames tiled into one JPEG/WebP-still image) decodes once at load, uploads as one texture, and animates by per-instance UV offset — exactly what the instanced-quad renderer already does. Animated WebP needs a CPU decoder ticking per visible tile; tiny MP4s reintroduce per-tile video decode — the problem Level 2 exists to avoid. 1s @ 15fps @ 240×135 ≈ a 1-megapixel JPEG.

### Media backend
Start simple: external `ffmpeg`/`ffprobe` invocation, or a bundled media crate if clean.

Later compare: direct ffmpeg bindings, platform-native decode, libmpv only for the selected preview.

### Cache lookup performance
Start with the filesystem cache. Only add SQLite if profiling proves:
- too many filesystem stats,
- slow startup over huge sets,
- search needs structured indexing,
- eviction becomes painful.

### Window/render separation
Begin with `sb-window` owning the surface and render loop (and the loop itself — see §12). Split `sb-render` only when the renderer becomes large enough to justify it.

---

## 16. Tech stack summary

- **Language:** Rust.
- **Input model:** stdin paths, newline or NUL-delimited, streaming.
- **Windowing:** `winit`.
- **GPU:** `wgpu`.
- **Cache:** filesystem-first.
- **Media:** swappable `sb-media` backend.
- **Preview:** external mpv first, internal later.
- **First platform:** macOS.
- **Product model:** fzf for videos, not a media manager.

---

## 17. One-line scope

**Switchblade is a fast GPU clip picker with cached thumbnails and a selected-clip preview. Everything else — including animated thumbnails — is v2 or optional.**

---

*Switchblade — flick it open, pick the shot.*
