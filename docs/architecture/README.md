# Architecture notes

How each subsystem actually works, **and the bugs that shaped it**. These
notes exist so hard-won behavior doesn't get re-broken: a paragraph
explaining why promotion runs before pruning, or why `-copyts` is
load-bearing, is worth more than the code comment it replaces. Treat the war
stories as specification, not history.

**Read the relevant file before changing that subsystem.**

| File | Covers |
|---|---|
| [live-playback.md](live-playback.md) | `SeekablePlayer`, the hires lane, warm-pool pre-warm, continuity handoff, focus pause, skip/seek |
| [media-pipeline.md](media-pipeline.md) | Worker queue tiers, ffmpeg/ffprobe invocation, sidecar cache & recipes, ingest gatekeeper + sorted ingest, atlas slots & eviction |
| [grid.md](grid.md) | Flexible/fixed layouts, per-clip springs, gripped-ribbon pinch zoom, zoom reflow & wrap |
| [modals.md](modals.md) | Quickview, fullview, filmstrip, seekbar/scrub/storyboard, chapter bar, backdrop freeze |
| [render-loop.md](render-loop.md) | Idle throttling, deadline-paced redraws, occlusion, upload budgets, user-attention guards |
| [selection-and-actions.md](selection-and-actions.md) | Random/shuffle/auto-skip, native drag-out, siblings swap |

Related: [DESIGN.md](../../DESIGN.md) (why the product is shaped this way) ·
[TASKS.md](../../TASKS.md) (what's still open) ·
[docs/perf-reviews/](../perf-reviews/) (measured performance reviews) ·
[benchmarks/HARNESS.md](../../benchmarks/HARNESS.md) (how to measure).
