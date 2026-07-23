# Switchblade — task queue

The single prioritized collection of open work. Everything actionable lives
here; everything reflective lives elsewhere:

- [DESIGN.md](DESIGN.md) — vision, pillars, settled decisions, spike records
  (don't re-litigate those here).
- [docs/perf-reviews/](docs/perf-reviews/) — numbered, chronological
  performance reviews (reflective; their open leftovers are indexed below).
- [benchmarks/HARNESS.md](benchmarks/HARNESS.md) — the sb-bench harness design
  record; its open phases are indexed below.
- [CLAUDE.md](CLAUDE.md) — shipped-behavior notes and hard rules for agents.

Conventions: **Now** is ordered — top item first. **Epics** (E1…) are
milestone-scale and carry their full spec here. Small tasks link to the doc
that owns their full problem statement instead of duplicating it. When
something ships, move its durable lessons into CLAUDE.md/DESIGN.md and delete
the entry — this file should only ever contain open work.

---

## Now

### 1. Attention-lane evaluation & verdict *(blocks E1+)*

The spike is BUILT (DESIGN.md §15) behind hot-reloadable
`interaction = "classic" | "attention"`; what remains is the **feel evaluation
and verdict**, and the verdict shapes the select model everything after builds
on. Evaluate:

- hover settle vs 1080p cold-spawn churn while sweeping the grid (the settle
  delay `attention_delay_ms` is the guard; it may need to be longer);
- keyboard-only warm-pool coverage (the pool can't predict the mouse —
  hover-to-first-frame stays a cold spawn; acceptable in feel?);
- misclick cost: every stray click is now a modal — does Esc-out feel cheap?
- measure vs classic with the `RUST_LOG=sb_app=debug` redraw-reason line +
  core usage on the same library.

**Exit:** a verdict — adopt (attention becomes default, classic possibly
deleted), keep both behind the flag, or reject with notes. Multi-select's
border-only `marked` state likely survives regardless.

### 2. P1.7 — Resolve the drag-ghost thumb path at press time *(trivial)*

Filesystem calls currently happen in the CursorMoved handler when a drag
matures. Stash the ghost path in `Press` at mouse-down. Full statement:
[perf review 02 § P1.7](docs/perf-reviews/02-efficiency-review.md).

---

## Epics (priority order)

### E1 — M9: metadata sort & filter *(next buildable milestone after the verdict)*

Reorder and subset the ingested grid by metadata, driven by **internal
commands bound to keys** — no UI chrome yet (`[keys]`/`[commands]`, DESIGN.md
§11). Needs no text stack, so it lands before M7.

Sorts (each toggles ascending/descending on repeat; a `sort_ingest` command
restores stdin/CLI order): `sort_created` (creation date), `sort_rating`,
`sort_size`.

Filters (each press cycles a mode, wrapping back to `all`):
`filter_resolution` — all → 1080p+ → 4K+; `filter_fps` — all → 30fps+ →
60fps+ → 120fps+.

**Data sources** — mostly already cached, which is why this is cheap:

- resolution and fps are already in `Meta` → free once probed; a
  not-yet-probed clip has no meta — decide its bucket (lean toward showing as
  "unknown" so an un-probed grid isn't empty).
- file size: from the `stat()` already done at fingerprint time.
- creation date: macOS `st_birthtime`, fall back to mtime. *Open: filesystem
  birthtime vs the container `creation_time` tag — the latter needs a new
  probe/`Meta` field; start with birthtime.*
- rating: **the library encodes stars in filenames** (`… ★★★★★.mp4`) — parse
  a trailing star run. *Confirm this is the canonical source before building;
  the alternative is an xattr or sidecar.*

**Design constraints:**

- **Stdin order stays sacred** (hard rule): sort/filter is a *view* over the
  ingested set, never a reordering of it. Keep the ingest vector authoritative
  and render from a separate ordered/filtered index list — the same
  **view-indirection layer** M7's fuzzy filter will reuse, so build it here.
- **Selection stays sane across changes**: track the selected clip by path,
  re-resolve after any sort/filter; if a filter hides it, fall to the nearest
  visible clip.
- Index-keyed machinery (warm pool, live lanes, slot owners) must key
  consistently off the *view* index, or off path where it already does (the
  D-swap `pending_reselect` path-matching is the precedent).
- An empty result is a valid state (draw an empty grid, don't crash).
- **Land [P1.1 (spring-work proportionality)](docs/perf-reviews/02-efficiency-review.md)
  with this epic** — the review's verdict was to build active spring state on
  M9's view indirection rather than as a standalone pass M9 would rework.

**Exit criteria:** a keybind flips the grid between all/1080p+/4K+ and
all/30/60/120fps+, and sorts by date/rating/size, with the selected clip
preserved and stdin order restorable — all without a text stack.

### E2 — M7: search/filter *(brings the text stack)*

- Fuzzy filename search; filter the current input set; keep selection sane
  across filters (reuses E1's view-indirection layer).
- Real text rendering lands here — the first time a text stack enters the
  codebase. The font dependency is the user's call (hard rule: ask before
  adding a dependency).

**Exit criteria:** large clip sets become practical.

### E3 — M10: hashtags

View a clip's hashtags and filter the grid by them.

- **Source (confirm before building):** filename tokens
  (`clip #loop #glitch.mp4`) parsed at ingest — same trick as E1's
  trailing-star rating. Alternatives: xattrs or a sidecar file.
- **Filtering** rides E1's view layer verbatim — just another predicate,
  selection tracked by path, empty result valid.
- **Viewing** tags needs real text, so the display half lands with/after E2;
  its natural home is E4's side drawers. A keybound tag-cycle filter could
  ship text-free before that — scoping call.

**Exit criteria:** a clip's hashtags are visible somewhere, and the grid can
be narrowed to one or more tags and restored, with selection preserved.

### E4 — M11: drawers

Dock-style edge reveal: push the pointer to a screen edge and a drawer slides
out; pull away and it retracts.

- **Bottom edge** → the chapter bar (edge-hover already exists in fullview;
  extend as the general pattern).
- **Left/right edges** → an info panel (name, resolution, fps, duration,
  size, date, rating) and a hashtag panel (E3's display surface).
- Reuses the filmstrip/chapter-bar slide machinery; reveal threshold and
  dwell/hide delays are `Tuning` fields — must never fire mid-pan/scrub.
- The info/hashtag panels need E2's text stack; the text-free bottom drawer
  lands first as the proving ground.

**Exit criteria:** resting the pointer at an edge slides the drawer out
smoothly (and never by accident mid-gesture); leaving retracts it.

---

## Conditional / deferred (each has an entry criterion — don't start early)

- **M8 straggler — denser seek strip** (`seek_16x1` artifact, own queue
  tier): only if the g² storyboard proves too coarse in use (DESIGN.md §14
  M8 phases 2–3).
- **Storyboard above visible thumbs while fullview hides the grid**: the
  remaining edge case from
  [chapter_sheet_latency](benchmarks/reports/chapter_sheet_latency.md) — a
  stone-cold library + instant fullview leaves chips waiting behind ~45
  hidden-grid thumb jobs. Needs a context signal (grid AND strip hidden), not
  a blanket tier swap. Deferred pending that decision.
- **P2 group** (full statements in
  [perf review 02](docs/perf-reviews/02-efficiency-review.md); measure first,
  none is a proven cost): P2.1 cache the quickview frosted backdrop ·
  P2.2 reuse frame-construction scratch buffers · P2.3 smaller grid artifact
  vs poster quality (atlas pressure) · P2.4 lazily allocated atlas pages
  (only if P0.5's answer stops sufficing) · P2.6 minor per-frame churn (fold
  into P2.2).
- **NV12 over the wire + GPU color conversion** (P2.5): deferred by
  [perf review 01](docs/perf-reviews/01-live-video-pipeline.md) — start only
  if profiling shows convert/upload ≥1 core or gaps on target hardware, or
  multiple simultaneous hires streams become a requirement.
- **Benchmark harness** ([HARNESS.md](benchmarks/HARNESS.md)): 2.2 cache
  seeding as a runner sub-mode · 3.4 thread-count leak canary · 3.5
  orchestrator TODOs (warm-up-discard opt-in, power/cold-axis labels,
  process-tree CPU/RSS) · 5.1 more scenarios (warm-pool advance, shuffle
  under load, focus-pause resume, external-drive I/O) · 5.2 Tier B `--bench`
  mode (present-to-present, upload bytes/frame, blur cost).

## Backlog (unscoped ideas — scope before committing)

- Still images as one-frame movies (extension whitelist; what `Meta`,
  quickview, seekbar, auto-skip mean for a still).
- `--wrap` infinite grid mode.
- Better thumbnail frame selection.
- Multi-select batch actions (the `marked` state from the attention spike is
  the foundation).
- Metadata search.
- Optional SQLite index — only if the filesystem cache provably hurts
  (DESIGN.md §8).
- Optional `sb-render` split; platform-specific decode backends.
- Post-fx flavor pass (scanlines/glow) via the reserved shader slot.
