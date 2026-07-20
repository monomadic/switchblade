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

> **REMOVED (2026-07-20, user decision):** grid sheet-cycling shipped (M6, animation level `full`) and was then cut — it didn't look good and its library-wide background sheet sweep competed with the thumb sweep and live playback for CPU. The sprite-sheet *artifact* survives as on-demand storyboard data only: quickview/fullview request the selected clip's sheet when opened (seekbar hover previews, chapter-bar chips), gated behind the playing stream's first frame, and clips never opened never generate one. The freed workers finish the static-thumb sweep sooner. Level `full`, the `a` toggle, and the bulk `anims` queue tier are gone ("full" still parses as `normal`). Don't re-add a library-wide sheet sweep without the user.

This was the "alive grid" feature, deliberately deferred: the grid's fluid motion is the priority, and it must feel great with static tiles first.

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
Configurable via `cache_key` (startup-only), two modes today:

```
"size_mtime"  size + mtime                   (default; survives renames/moves)
"path"        absolute path + size + mtime   (original MVP key)
```

> **Tradeoff:** `path` keying means moved/renamed files lose their cache — and clip libraries get reorganized a lot (rating-star renames especially), so `size_mtime` is the default: it drops the path so those entries survive. Its only cost is that two distinct files sharing an exact byte size AND mtime second collide (rare; a wrong thumb until `--cleanup-cache`), and exact duplicates deliberately share one entry. Switching to `size_mtime` **adopts** existing path-keyed entries by renaming them across on first access — no library-wide regeneration — and `--cleanup-cache` treats an entry as live under *either* keying, so a config flip never eats a still-valid cache. `cached_meta` heals the stranded `meta.src` path on first read (cleanup judges liveness by it). Still deferred: `size + mtime + partial hash` (one cheap 64KB read, disambiguates the collision) or full content hash. Store the source path in `meta.json` regardless, for debugging.

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

> **Progress:** M0–M6 and M8 are shipped. The **attention-lane interaction spike** (§15 — hover/selection-following hires lane, click-to-quickview, cmd/shift-click multi-select) is built behind the `interaction` flag; its feel evaluation and verdict are next (the verdict shapes the select model everything else builds on), then M9, then M7 (which brings the text stack), then the planned M10 (hashtags) and M11 (drawers). A number of features landed beyond the numbered milestones — internal quickview + fullview modals, filmstrip, chapter bar, auto-skip, shuffle/random, native drag-out, siblings swap (`D`), flexible justified grid layout — see CLAUDE.md's Status section for the authoritative shipped-behavior notes.

### M0 — Skeleton ✅
- CLI accepts stdin paths (newline and NUL-delimited), **streaming** — don't wait for EOF.
- Window opens.
- Fullscreen toggle.
- Logs ingested paths.

**Exit criteria:** `fd -e mp4 . ~/Clips | switchblade` opens a window and receives paths as they arrive.

### M1 — Fake grid ✅
- Render placeholder tiles.
- Keyboard navigation.
- Pointer pan with inertia.
- Selection state.
- Smooth motion.
- Hot tuning config.

**Exit criteria:** the grid feels good even with fake tiles.

### M2 — Real file tiles ✅
- Tiles appear as paths stream in; preserve stdin order; stable placement.
- Debug-quality labels only (bitmap font or window-title filename — no text stack, see §9).
- Handle missing/unreadable files gracefully.

**Exit criteria:** a real piped file list becomes a navigable visual grid, still without thumbnails.

### M3 — Static thumbnail cache ✅
- Generate static thumbnails in background, prioritized by visibility/proximity.
- Store in filesystem cache.
- Load cached thumbnails on restart.
- Never block the render thread.

**Exit criteria:** second launch over the same files feels instant.

### M4 — External open/actions ✅
- Open selected clip in mpv.
- Basic configurable commands.
- Copy path.
- Reveal in Finder (optional).

**Exit criteria:** Switchblade is already useful as a visual picker.

### M5 — Selected preview ✅
- `Space` previews the selected clip.
- MVP may spawn mpv (with a distinct preview profile — loop, borderless).
- Later: internal overlay preview.

**Exit criteria:** you can skim, select, preview, and open without leaving the flow.

---

**═══ MVP complete at M5. Everything below is MVP v2. ═══**

---

### M6 — Animated thumbnails *(MVP v2)* ✅
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

### M8 — Quickview scrub *(MVP v2)* ✅ *feature-complete (2026-07)*
Pointer-driven seeking + filmstrip feel in the quickview modal; seeking hits **only the quickview main video** (grid never seeks). Every live lane rides the in-process `SeekablePlayer` (§15). Shipped:
- **Seekbar** — pointer motion over the video reveals a slim bar (shares the `[`/`]` skip flash's drawing), fades after `seekbar_hide_s`; thickens under the pointer. Click seeks; hold-drag scrubs two-phase like mpv — keyframe seeks while dragging (9–30ms), one exact seek on release (GOP-bound, hidden behind the shown keyframe frame). *(`seekbar_click_and_drag_scrubs_the_stream`)*
- **Storyboard hover thumbs (phase 1)** — hovering the bar draws the anim-sheet cell nearest that timestamp; quickview requests the selected clip's sheet on demand (since 2026-07-20 this on-demand path is the ONLY sheet generation — see the Level 2 removal note).
- **Filmstrip** — chips hover-play instantly (settle delay dropped in quickview); wheel/trackpad scrubs the strip and commits selection to the nearest chip while the backdrop grid stays put. *(`filmstrip_scroll_commits_selection`)*
- Tuning: `seekbar_hide_s/fade_ms/height/hover_height/thumb_width`, `strip_scroll_sensitivity`.

Remaining, conditional (only if the g² storyboard proves too coarse in use): *phase 2* — a dedicated denser strip (`seek_16x1_<w>x<h>_q<q>.jpg`, seeked single-frame extracts like the anim recipe, never an `fps=` decode) as its own queue tier, generated on first quickview of a clip (on-demand quickview work rides `anims_now` above the gen sweep — a starved storyboard taught us user-attention jobs never park below library-wide tiers); *phase 3* (only if still coarse) — exact frames on demand from a second **paused low-res `SeekablePlayer`** on the same clip (the in-process port makes thumbfast's paused-slave trick a two-line reuse instead of a process to babysit).

**Exit criteria:** met — the pointer alone finds a moment (bar + hover thumbs + click), the filmstrip feels physical, and chained seeks never flash the thumbnail (worst case is freeze-then-jump).

### M9 — Metadata sort & filter *(MVP v2)*
Reorder and subset the ingested grid by metadata, driven by **internal commands bound to keys** — no UI chrome yet (`[keys]`/`[commands]`, per §11). Needs no text stack, so it can land before M7.

Sorts (each toggles ascending/descending on repeat; a `sort_ingest` command restores stdin/CLI order):
- `sort_created` — creation date.
- `sort_rating` — rating.
- `sort_size` — file size.

Filters (each press cycles a mode, wrapping back to `all`):
- `filter_resolution` — all → 1080p+ → 4K+.
- `filter_fps` — all → 30fps+ → 60fps+ → 120fps+.

**Data sources** — mostly already cached, which is why this is cheap:
- resolution (`width`/`height`) and `fps` are already in `Meta` → the filters are free once a clip is probed; a not-yet-probed clip has no meta, so decide its bucket (show as "unknown", or hide until meta arrives — lean toward showing so an un-probed grid isn't empty).
- file size: from the `stat()` already done at fingerprint time.
- creation date: macOS `st_birthtime`, fall back to mtime. *Open: filesystem birthtime vs the container `creation_time` tag — the latter needs a new probe/`Meta` field; start with birthtime.*
- rating: **the library encodes stars in filenames** (`… ★★★★★.mp4`, `★★★★☆`) — parse a trailing star run. *Confirm this is the canonical source before building; the alternative is an xattr or sidecar.*

**Design constraints:**
- **Stdin order stays sacred** (hard rule): sort/filter are a *view* over the ingested set, never a reordering of it. Keep the ingest vector authoritative and render from a separate ordered/filtered index list — the same view-indirection M7's fuzzy filter will want, so build it here and reuse it there.
- **Selection stays sane across changes** (like M7): track the selected clip by path, re-resolve its position after any sort/filter; if a filter hides it, fall to the nearest visible clip.
- Index-keyed machinery (warm pool, live lanes, slot owners) must key consistently off the *view* index, or off path where it already does (the D-swap `pending_reselect` path-matching is the precedent).
- An empty result is a valid state (draw an empty grid, don't crash).

**Exit criteria:** a keybind flips the grid between all/1080p+/4K+ and all/30/60/120fps+, and sorts by date/rating/size, with the selected clip preserved and stdin order restorable — all without a text stack.

### M10 — Hashtags *(planned)*
View a clip's hashtags and filter the grid by them.
- **Source (confirm before building):** filename tokens (`clip #loop #glitch.mp4`) parsed at ingest, the same trick as M9's trailing-star rating — cheap, no probe, survives the cache. Alternatives: xattrs or a sidecar file.
- **Filtering** rides M9's view-indirection layer verbatim — a hashtag predicate is just another filter over the sacred ingest vector, selection tracked by path, empty result valid.
- **Viewing** tags on a clip needs real text, so the display half lands with/after M7's text stack; its natural home is the side drawer panels (M11). A keybound tag-cycle filter could ship text-free before that — scoping call.

**Exit criteria:** a clip's hashtags are visible somewhere, and the grid can be narrowed to one or more tags and restored, with selection preserved.

### M11 — Drawers *(planned)*
Dock-style edge reveal: push the pointer to a screen edge and a drawer slides out; pull away and it retracts.
- **Bottom edge** → the chapter bar (today `g`-only; edge-hover becomes a second way in, in fullview/quickview first).
- **Left/right edges** → an info panel (name, resolution, fps, duration, size, date, rating) and a hashtag panel (view + toggle tag filters — M10's display surface).
- Reuses the filmstrip/chapter-bar slide machinery (strip springs, slide-damped overlaps); reveal threshold and dwell/hide delays are `Tuning` fields — must not fire during ordinary pans or scrubs.
- The info/hashtag panels need the text stack (M7); the bottom drawer (chapter bar) doesn't, so it can land first as the proving ground for the edge-hover gesture.

**Exit criteria:** resting the pointer at an edge slides the drawer out smoothly (and never by accident mid-gesture); leaving retracts it; the bottom drawer is the existing chapter bar.

### Later
- Still images as one-frame movies: an image file ingests like a clip whose thumb IS the image (no live lane, no sheet, duration 0/undefined). To be scoped and considered before committing — extension whitelist, what `Meta` looks like without a probe, and what quickview/seekbar/auto-skip mean for a still.
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

### Attention-lane interaction spike *(BUILT — evaluation pending, before M9)*

> **Progress (2026-07):** the model below is implemented behind the hot-reloadable `interaction = "classic" | "attention"` flag (plus `attention_delay_ms`, the hover-settle guard — default 250ms, deliberately longer than `live_delay_ms`). Attention retargets the existing selected-stream lane (`attention_target()`: hover while mousing via a `mouse_attention` modality bit, selection while keyboard-navigating; strict — a gap hover plays nothing); the grid's tile-size hover lane is gated off; click-anywhere quickviews on mouse-up by promoting the lane; cmd/shift-click marks `marked` (border-only, shuffle-remapped, D-swap-cleared, Esc-dropped). Modifiers arrive as `Mods` on `MouseDown` (sb-window tracks `ModifiersChanged`). Classic is unchanged. What remains is the **evaluation + verdict** per the risks below.

Rework the grid's core gesture around a single **attention lane** and see how it feels and performs before committing. Flag-gated (`interaction = "classic" | "attention"`, hot-reloadable if cheap) so the two models can be compared in place.

**The model:**
- **One hires lane follows attention**: the hovered tile while mousing, the selected tile while keyboard-navigating. It decodes at quickview resolution exactly like today's selected lane (the tile samples it downscaled) — so it costs what the selected lane costs now, and today's tile-size hover lane is deleted.
- **Click opens quickview immediately** by promoting the attention lane's already-running stream — the same zero-handoff trick quickview uses today, minus one click. Composes with drag-out unchanged (open already fires on mouse-up; a matured drag suppresses it).
- **Cmd-click / shift-click select** (toggle / range): selected clips get the border + selected state only — they never play. Multiple selection allowed; this is the multi-select foundation the Later list wants, and actions/batch actions target it.
- **Strict playback rule:** the attention lane is the only thing that ever plays in the grid. (A "last-selected keeps playing over empty space" rule can be added later if strict feels dead.)
- Comparing two clips = quickview's filmstrip, or cmd-click the first and seek out the second.

**Why it might win:** click-to-preview is more direct than select-then-Space; one decoder instead of two (small CPU/VT saving during hover); the interaction and the playback architecture collapse into one concept.

**Spike risks / what to actually evaluate:**
- Hover is more volatile than selection — every settle now spawns a 1080p decoder, not a tile-size one. The settle delay is the guard; it may need to be longer in this mode. Watch cold-spawn churn while sweeping the grid.
- The warm pool pre-warms keyboard destinations and can't predict the mouse — hover-to-first-frame stays a cold spawn. Is that acceptable in feel?
- Misclick cost: every stray click is now a modal. Does Esc-out feel cheap enough?
- Measure with the `RUST_LOG=sb_app=debug` redraw-reason line + core usage vs classic on the same library.

**Exit criteria:** a verdict — adopt (attention becomes the default, classic possibly deleted), keep both behind the flag, or reject with notes. Multi-select's border-only state likely survives regardless of the verdict.

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

> **Resolved for live playback (2026-07):** direct ffmpeg bindings (in-process libav) — see "Low-latency seek" below; all live lanes run on it now. External CLI invocation stays for thumbs/probes/sheets.

### Low-latency seek (quickview) ✅ *shipped 2026-07*

The M8 prerequisite. **Problem:** the ffmpeg-CLI `LivePlayer` had no seek channel — its pipe carries only frames — so every seek was a process respawn (~1s floor on 4K: exec + probe + VT session init + GOP decode, unfixable with flags). The quickview tile/modal sharing one decoder was never the cost (that's why quickview *opens* instantly). Browsers and mpv seek in tens of ms because they keep a **persistent demuxer + decoder session** alive (seek = sample-table lookup → nearest keyframe → partial GOP).

**Decision:** own the demuxer + decoder in-process via libav (`rsmpeg`), session + index resident. Rejected — latency-hiding pre-warm (floor is still the cold spawn), paused-mpv slave (a process to babysit; but see M8 hover-thumb phase 3), AVFoundation (macOS-only, diverges from ffmpeg color math). Benchmarked before committing (`spikes/seek-bench`, rsmpeg 0.18 vs brew ffmpeg 8):

| | session init | keyframe seek | exact seek (avg / worst) |
|---|---|---|---|
| resident VT, 4K h264 (long GOP) | 10ms once | 20–30ms | 311 / 592ms |
| resident VT, 4K60 HEVC | 2ms once | 9–16ms | 121 / 196ms |
| CLI respawn (old model) | per seek | — | 280–880ms every time |

Exact seeks are GOP-decode-bound (~7ms/frame under VT), so long-GOP 4K can't hit <100ms exact — which **settles the interaction design, mpv-style: scrub = keyframe seeks (instant), release/settle = one exact seek** (hidden behind the keyframe frame already on screen). VT stays load-bearing; VP9/AV1 keep sw decode (keyframe-snappy, exact-slow — acceptable).

**Shipped as `SeekablePlayer`** (`crates/sb-media/src/seekable.rs`): the same paced-queue contract `LivePlayer` presents (`spawn`/`take_frame`/`buffered`) plus `seek(f64, exact)`; demuxer + decoder owned by the reader thread, `seek()` = flush + `avformat_seek_file` backward (+ decode-forward when exact), position is a real pts property. The decode/scale chain reuses `hw_scale_vf` verbatim as a libavfilter graph (parity by construction), the sw fallback rotates explicitly (libavfilter doesn't autorotate), pacing stamps by pts-delta with late re-anchor. All live lanes — selected, warm pool, hover — run on it; `[`/`]` is a plain `seek()` and the old `warm_skip`/checkpoint machinery is deleted. `LivePlayer` stays in sb-media (tests + pacebench keep it compiling) as an instant-revert fallback until a release soaks, then goes. Tests: the `SeekablePlayer` pacing/stall/drop trio + `seek_jumps_in_place_without_respawn` + `skip_seeks_the_selected_stream_in_place`.

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
