# Live-video performance — status & durable facts

Companion to [PLAN.md](PLAN.md), covering live-playback performance.
**Phases 0–4 shipped 2026-07-10** (investigated on an Apple M3, 8-core
fanless, 60Hz panel, ffmpeg 8.1.1 Homebrew); **Phase 5 is deferred**
(below). The per-lane player later moved from the ffmpeg-CLI `LivePlayer`
to the in-process `SeekablePlayer` (PLAN.md §15, 2026-07); the hw/sw scale
chains and pacing invariants below carried over verbatim.

## Root cause (measured, not theorized)

The pacing model was always sound — the ffmpeg *producer* couldn't feed
it. Library is ~78/400 files 4K, dominated by 3840×2160@60 HEVC.
VideoToolbox *decode* is fine (113fps); the software `scale=2560:1440` +
YUV→RGBA (swscale, CPU) capped the chain at **67fps / ~3.7 cores** — no
headroom, and it collapsed under the app's own concurrency (warm fills,
hover lane, selected tile). On the fanless M3, sustained multi-core load
also thermally throttles. When the producer runs below meta fps, the
reader's re-anchor plays each frame as it decodes → slow-motion judder;
skip-to-newest after stalls → speed-ups. Ruled out: fps meta (accurate),
VFR (files are true CFR), render pacing, re-anchor drift.

**The fix** (Phase 1) moved scaling onto the GPU (`scale_vt`), cutting the
per-stream cost ~4×. The VT media engine saturates at ~2 concurrent
flat-out 4K60 chains, which is exactly what the warm-pool design keeps it
under by construction (serialized fills, backpressure-stalled parking).

## Measurements (M3 Air, ffmpeg 8.1.1, cool machine on AC)

480 frames 4K60 HEVC, decode → scale 2560×1440 → RGBA:

| chain | wall fps | CPU cores |
|---|---|---|
| VT decode only | 113 | ~0.3 (decode was never the problem) |
| swscale bicubic → RGBA (old default) | 67 | ~3.7 (the bottleneck) |
| swscale `fast_bilinear` → RGBA (sw relief, Phase 2) | 100–150 | ~2.1–3.0 |
| **`scale_vt` hw + nv12→rgba @1440p (Phase 1 fix)** | **128–144** | **~0.9** (10-bit ~2.3) |

pacebench (60Hz polling) after Phases 1–3: 8-bit / 10-bit / VP9-vertical /
rot±90 all **60.00fps, 0 gaps**; contended (paced stream + one flat-out hw
chain, the selection-move condition) **59.75fps, 1 gap/12s**. A 3-chain
overload collapses (40fps, 10 gaps/s) — hence the ≤2-active-chains design.
Phase 4 verdict: no warm-pool tuning needed (user-validated in-app);
`set_parked` + sb-media's `libc` dep deleted as unused.

## Durable ffmpeg gotchas (the hw scale chain)

Load-bearing facts (also in CLAUDE.md's live-playback bullet):
- Output format name is **`videotoolbox_vld`**, not `videotoolbox`.
- `hwdownload` needs the raw format named explicitly, gated by bit depth:
  `nv12` (8-bit) / `p010le` (10-bit). `format=rgba` direct fails;
  `nv12|p010le` mis-negotiates on 10-bit.
- hw frames do **not** autorotate — ±90/±270 → explicit
  `transpose_vt=clock`/`cclock` (PSNR-verified both signs); 180°/odd →
  sw fallback (which autorotates).
- Scale/hwdownload dims must be **mod-8** (not just even) or delivery
  jitters ~2× periodically; `spawn` rounds down, content squeezes ≤7px,
  never crops. Callers read actual dims off `player.w/h`.
- Only h264/hevc/prores go through VT (`vt_accel`); VP9/AV1 decode
  *slower* through it → sw chain. 10-bit HLG/PQ comes out un-tonemapped
  (parity with sw, out of scope).
- Color parity hw-vs-sw: 33dB PSNR (scaler diff only, no gamma drift) —
  the final `format=rgba` conversion at target res keeps live-video color
  math identical to the thumbnail path (the gamma-pop regression class).

## Phase 5 — DEFERRED: NV12 over the wire + GPU color conversion

**Do not start until an entry criterion is met.** After Phase 1 the
remaining CPU convert (~0.7 core at 4K60) and the 14.7MB/frame RGBA copy
(now in `SeekablePlayer::push_rgba`) are no longer the bottleneck — the
win shrank from "unlocks realtime" to "saves a fraction of a core,"
against the project's worst regression class: thumbs decode to RGBA *via
ffmpeg* precisely so live and thumbnail color math match, and a
shader-side YUV→RGB must reproduce BT.601/709 × limited/full-range per
clip or the thumb→live gamma pop returns. It also touches the App
boundary (`HiresFrame` is RGBA), the renderer (two-plane R8+RG8 textures +
new `tiles.wgsl` sampling), and leaves the atlas hover lane RGBA
regardless (two paths forever).

Entry criteria (any): profiling shows convert/upload ≥1 core or gaps on
target hardware; **the edge-peek milestone lands** (do both
hires-texture-format disruptions in one renderer change); or multiple
simultaneous hires streams become a requirement.

Sketch: output `nv12` (5.5MB/frame at 1440p, 2.67× less than RGBA); upload
R8 luma + RG8 chroma; convert in `tiles.wgsl` with matrix+range from meta
(add `color_space`/`color_range` to `Meta` alongside `pix_fmt`); verify
with the PSNR-vs-thumbnail check and a BT.601/709/full-range clip matrix.

## Standing rules for this work

- Benchmarks run **serially, idle machine, AC power**; note machine +
  ffmpeg version with any recorded number. Harness is `pacebench`
  (`cargo run --release -p sb-media --example pacebench -- <clip> <w> <h> <fps> [secs] [sw]`;
  deterministic asset-gen commands live in its doc comment). Discard each
  clip's first (cold) run; use ≥8s assets (short clips gap on
  `-stream_loop` restarts).
- Every perf change re-runs the sb-media regression tests: the four
  `LivePlayer` ones (`live_player_paces_frames`,
  `unwatched_player_stalls_then_serves_instantly`,
  `dropped_player_releases_its_reader`, `anim_sheet_generates_and_tiles`)
  plus the `SeekablePlayer` mirror trio and
  `seek_jumps_in_place_without_respawn`.
- Pacing invariants (CLAUDE.md) still bind: promotion before pruning;
  bounded queue = free warmth; drop must wake the condvar.
