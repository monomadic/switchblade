# Live-video performance refactor plan

Companion to [PLAN.md](PLAN.md) — this doc covers only the live-playback
performance work. Execute phases in order; each is independently landable
and independently verifiable. Check boxes as work lands, and record fresh
measurements in the tables so regressions are visible.

## Context

Symptoms: live tiles and quickview play too fast / too slow / stuttered,
even with no background jobs. Investigated 2026-07-10 on the reference
machine (Apple M3, 8-core fanless, 60Hz panel, ffmpeg 8.1.1 via Homebrew).

**Root cause (measured, not theorized):** the `LivePlayer` pacing model is
sound, but the ffmpeg producer chain feeding it cannot sustain the
library's dominant content. The library is ~78/400 files 4K, dominated by
**3840×2160@60 HEVC**. For those, VideoToolbox decode is fine (113fps) but
the software `scale=2560:1440` + YUV→RGBA conversion (swscale, CPU) caps
the chain at **67fps while consuming ~3.7 cores** — no headroom. Measured
through the real `LivePlayer` with a 60Hz consumer on an idle machine:
**51fps delivered vs 60 target, ~4 visible gaps/sec, worst gap 86ms.**
Under concurrent load (which the app self-inflicts: warm fills at full
quickview resolution after every selection move, hover lane, grid selected
tile playing the hires stream) it collapses — 11fps with 1s freezes was
measured under an 8-core competing workload. Sustained multi-core load on
the fanless M3 also builds heat → thermal throttle → worse over minutes.

When the producer runs below the meta fps, the reader's re-anchor logic
(correctly) plays each frame as it decodes: slow motion + decode-jitter
judder. Skip-to-newest after stalls reads as speed-ups. Ruled out: fps
metadata (sampled 400 meta.json — accurate), VFR (spot-checked files are
true CFR; avg_frame_rate off ≤0.2%), render-loop pacing, re-anchor drift.

## Baseline measurements (2026-07-10, idle M3)

480 frames of 4K60 HEVC (`testsrc2`, hevc_videotoolbox 60Mbps), decode →
scale to 2560×1440 → RGBA:

| chain | wall fps | CPU cores | notes |
|---|---|---|---|
| VT decode only | 113 | ~0.3 | decode was never the problem |
| **current: swscale bicubic → RGBA** | **67** | **~3.7** | the bottleneck |
| swscale `flags=fast_bilinear` → RGBA | 100 | ~3.0 | sw-path relief |
| **`scale_vt` hw + nv12→rgba @1440p** | **128** | **~1.5** | the fix |

End-to-end through `LivePlayer` (pacebench, 60Hz polling, idle machine):

| lane | delivered | gaps >1.8× | p50/p90/p99 interval ms |
|---|---|---|---|
| quickview 2560×1440 @60 | 51.0 fps | 3.9/s, max 86ms | 17.5 / 28.0 / 40.4 |
| tile 960×540 @60 | 59.9 fps | 0 | 16.7 / 17.5 / 20.6 |

**Thermal/power caveat (Phase 0 re-measurement, same day, cool machine on
AC):** the same swscale chain hit 151fps max throughput at the same ~3.7
cores, and pacebench-alone ran clean (60.0fps, 0 gaps, p90 18.6ms). The
67/51fps numbers above reflect a heat-soaked or battery-throttled state —
which IS a state the fanless Air reaches in real use. The durable facts
are the **per-stream cost (~3.7 cores)** and what it causes under the
app's own concurrency: with just ONE competing warm-fill chain (what
every selection move spawns), the cool machine already degrades to
59.2fps with 0.8 gaps/s, p99 33ms. Phase 1 acceptance therefore includes
a contended run, and core-cost is the primary metric, not idle fps.

Pre-flight facts for Phase 1 (verified on ffmpeg 8.1.1):
- `-hwaccel_output_format videotoolbox` is rejected; the name is
  **`videotoolbox_vld`**.
- `hwdownload` requires an explicit raw format matching the hw frames:
  `format=nv12` for 8-bit, `format=p010le` for 10-bit. `format=rgba`
  directly fails; `format=nv12|p010le` mis-negotiates on 10-bit. **The
  chain must be gated by source bit depth.**
- **Autorotation does NOT happen on hw frames**: rotated clip through the
  hw chain scored PSNR ~4.5dB vs the sw path's (auto-rotated) ground
  truth. Needs explicit `transpose_vt` or sw fallback.
- 10-bit HLG/PQ comes out un-tonemapped — flat colors. Same as today's sw
  path (parity, not a regression). Out of scope.

---

## Phase 0 — land the measurement harness ☑ (2026-07-10)

Goal: make every later phase's acceptance criteria a runnable command.

1. ✅ Landed as `crates/sb-media/examples/pacebench.rs` (spawns a
   `LivePlayer`, polls `take_frame` every 16.67ms like the render loop,
   reports delivered fps, interval percentiles, and gap count; `probe`
   made pub so the bench reads codec/rotation without a cache entry).
   Usage: `cargo run --release -p sb-media --example pacebench -- <clip> <w> <h> <fps> [secs] [sw]`
2. ✅ Asset-generation commands live here and in the example's doc
   comment (they're deterministic):
   ```sh
   ffmpeg -y -f lavfi -i "testsrc2=duration=8:size=3840x2160:rate=60" \
     -c:v hevc_videotoolbox -b:v 60M -pix_fmt yuv420p -tag:v hvc1 4k60.mp4
   ffmpeg -y -f lavfi -i "testsrc2=duration=8:size=3840x2160:rate=60" \
     -c:v hevc_videotoolbox -profile:v main10 -b:v 30M -pix_fmt p010le -tag:v hvc1 4k60_10bit.mp4
   ffmpeg -y -display_rotation 90 -i 4k60.mp4 -c copy rot90.mp4
   ```
3. Record this machine's baseline in the tables above (done for M3 Air).

Acceptance: pacebench reproduces the baseline table within noise (±10%).
**Run benchmarks strictly serially — concurrent runs contaminated results
during the investigation and will again.**

## Phase 1 — hardware scaling (`scale_vt`) for VT codecs ☑ (2026-07-10)

**Landed & measured (M3 Air, ffmpeg 8.1.1, cool machine on AC):**

| run | result |
|---|---|
| hw chain flat-out 4K60 8-bit → 1440p | **144fps at 0.88 cores** (was 151fps at 3.7 — 4.2× less CPU) |
| hw chain flat-out 10-bit | 122fps at 2.3 cores (16-bit convert is pricier; still 2× realtime) |
| pacebench 8-bit 2560×1440 @60 | 60.00fps, 0 gaps, p99 20.0ms |
| pacebench 10-bit 2560×1440 @60 | 60.00fps, 0 gaps, p99 19.7ms |
| pacebench contended (+1 hw chain) | 59.75fps, 1 gap/12s (old chain: 9 gaps, p99 33ms) |
| pacebench rot90 / rot-90 810×1440 | 59.7 / 59.9fps, ≤0.3 gaps/s |
| rotation PSNR vs sw ground truth | +90→`cclock` 31.4dB, −90→`clock` 31.9dB (wrong sign: 0.25dB) |
| color parity PSNR hw vs sw, 8-bit | 33.3dB (scaler difference, no color/gamma drift) |

Two discoveries that amended the plan:
- **Dims must be mod-8, not just even.** Non-aligned scale_vt/hwdownload
  dims deliver with periodic ~2× interval jitter at fine throughput
  (810×1440: 4.4 gaps/s; mod-4 cleans 8-bit but 10-bit needs mod-8).
  `spawn` rounds hw-path dims DOWN to multiples of 8 — content squeezes
  ≤7px, never crops; callers read actual dims off `player.w/h`.
- **Benchmark clips must be ≥8s**: the 2s 10-bit asset gapped 2/s purely
  from `-stream_loop` restarts (regenerate with `duration=8`).
- **Discard each clip's first run**: cold file access reads as a gap
  burst (a real library 4K60 clip showed 2.4 gaps/s cold, then 0.0 on
  both warm reruns — 60.00fps, p99 20ms). Real-content validation passed
  on a library 2160p60 HEVC 22Mbps clip at 2560×1440.

Goal: quickview lane ≥59fps on 4K60 HEVC/H264 with ≥2 cores of headroom.
This is the highest-impact change; everything else is polish by comparison.

Touch points: `crates/sb-media/src/lib.rs` (`probe`, `Meta`,
`LivePlayer::spawn`, `vt_accel`), `crates/sb-app/src/lib.rs`
(`start_sel_live` / `start_live` pass new spawn params).

1. **Record `pix_fmt` in `Meta`** (`Option<String>`, `#[serde(default)]`;
   ffprobe already returns it in `-show_streams` — parse
   `streams[video].pix_fmt`). New probes get it for free.
2. **Old-cache self-healing:** `meta.json` written before this change
   lacks `pix_fmt`, and the cache never expires. When a live spawn finds
   `pix_fmt` absent, use the **software chain for that spawn** and queue a
   background niced re-probe that rewrites `meta.json` (a new low-priority
   request type on the existing worker queue, or piggyback on `Gen`). Next
   spawn of that clip goes hardware. No render-thread ffprobe (a blocking
   probe would hitch the UI).
3. **Spawn chain** — when `vt_accel(codec)` AND pix_fmt is a known 8- or
   10-bit 4:2:0 format AND rotation is 0 or ±90/±270:
   ```
   -hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld
   -vf [transpose_vt=...,]scale_vt=W:H,hwdownload,format=<nv12|p010le>,format=rgba
   ```
   - `nv12` for `yuv420p`/`nv12`, `p010le` for `yuv420p10le`/`p010le`.
     Anything else (4:2:2, 4:4:4, exotic) → software chain.
   - **Force W/H to multiples of 8** (`dw & !7`) — even dims are NOT
     enough; see the mod-8 discovery above.
   - **Rotation:** hw frames do not autorotate (verified). Map meta
     rotation ±90/±270 → `transpose_vt=clock`/`cclock` inserted before
     `scale_vt`; verify direction with the PSNR check below (both signs —
     don't trust the mapping from memory). 180° or odd angles → software
     chain (sw autorotates correctly).
   - Software chain remains the universal fallback (VP9/AV1, unknown
     pix_fmt, weird rotation, spawn failure).
4. **Color check** (the gamma-pop regression class this repo fought
   before): the final `format=rgba` swscale conversion happens at 1440p
   with frame props carried through, so color math should match the sw
   path. Verify: PSNR of one frame, hw chain vs sw chain, on a normal
   8-bit clip — expect >30dB (scaler algorithms differ slightly; gross
   color/gamma drift shows up as <20dB). Also eyeball a paused live frame
   against its thumbnail in-app. If tinted: pin `scale_vt`'s
   `color_matrix` option.

Verification:
```sh
cargo run --release -p sb-media --example pacebench -- 4k60.mp4 2560 1440 60        # ≥59fps, 0 gaps
cargo run --release -p sb-media --example pacebench -- 4k60_10bit.mp4 2560 1440 60  # ≥59fps, 0 gaps
# rotation ground truth (repeat for -90):
ffmpeg -y -hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld -ss 1 -i rot90.mp4 \
  -frames:v 1 -vf "<hw chain>" hw.png
ffmpeg -y -hwaccel videotoolbox -ss 1 -i rot90.mp4 -frames:v 1 -vf "scale=WxH" sw.png
ffmpeg -i hw.png -i sw.png -filter_complex psnr -f null -   # average ≥30dB
```
Acceptance: both pacebench runs ≥59fps with 0 gaps; **chain core cost
≤1.5 cores** (`/usr/bin/time`, user+sys ÷ real on a flat-out run);
**contended run** (pacebench + one concurrent hw chain, the
selection-move condition) ≥59fps with ~0 gaps; rotation PSNR ≥30dB both
directions; no visible color pop between thumb and live.

## Phase 2 — software-path relief ☑ (2026-07-10)

**Landed & measured:** sw chain (hw decode + `fast_bilinear` scale)
flat-out on 4K60 → 1440p: 150fps at **2.1 cores** (bicubic: 150fps at
3.7 on the same cool machine — 1.6 cores saved at identical throughput).
VP9 vertical 1080×1920@60 → 810×1440 pacebench: 60.00fps, 0 gaps.

Goal: cheaper fallback path for VP9/AV1 and anything Phase 1 rejects.

1. Add `flags=fast_bilinear` to the `scale=` filter in the software chain
   (`LivePlayer::spawn`). Measured: 67 → 100fps, ~3.7 → ~3.0 cores on the
   4K60 case. Bilinear at video rates is visually fine (frames persist
   ~17ms and the hires texture is mip-sampled anyway).
2. Leave thumb/anim generation untouched (quality matters there, rate
   doesn't).

Acceptance: sw-chain max throughput ≥90fps on 4k60.mp4 (forced sw by
passing `codec=None` through pacebench or a temporary flag); VP9 vertical
1080×1920@60 pacebench clean (≥59fps, 0 gaps).

## Phase 3 — pacing correctness hardening ☑ items 1–2 (2026-07-10)

**Landed:** `-r <fps>` CFR forcing (verified: a 60fps clip deliberately
paced at 30 delivers exactly 30.00fps, 0 gaps — dup/drop at correct
wall-clock speed, no more wrong-speed failure mode) and the
clone-out-of-lock fix (fresh buffer swapped in outside the mutex). All
four regression tests pass; pacebench unchanged-or-better on every lane
(8-bit/10-bit/VP9 all 60.00fps, 0 gaps, runs crossing a `-stream_loop`
boundary included). Item 3 (half-tick lookahead) intentionally NOT done:
p50 sits at exactly 16.7ms — no aliasing judder to fix.

Independent small fixes in `crates/sb-media/src/lib.rs`; land separately.

1. **Force CFR output with `-r <fps>`** on the output side of the spawn.
   Today the pacing is open-loop: the reader stamps frames 1/fps apart and
   *assumes* ffmpeg emits exactly that cadence. `-r` makes ffmpeg dup/drop
   to that cadence, so the assumption becomes true by construction. This
   converts the failure mode of wrong/missing fps meta and genuinely-VFR
   sources from "plays at the wrong SPEED" (bad) to "plays at the right
   speed with dup/dropped frames" (fine). Regression-check with the
   existing `live_player_paces_frames` test.
2. **Stop copying the frame under the queue mutex** (`spawn`'s reader
   loop: the lock guard is created before `buf.clone()` is evaluated in
   `push_back((due, buf.clone()))`). At 1440p that's a 14.7MB memcpy
   (1–2ms) inside the mutex `take_frame` contends on every render tick.
   Fix: build the frame Vec before locking (clone outside the lock, or
   swap in a fresh buffer each iteration and push the filled one).
3. *(Optional polish)* `take_frame` half-tick lookahead: 60fps dues vs
   60Hz polling alias (baseline p50 17.5ms vs 16.7 expected). Accept a
   frame due within the next half render interval (caller passes its dt).
   Only bother if judder is still perceptible on 60fps content after
   Phases 1–2.

Acceptance: all four existing `sb-media` regression tests still pass;
pacebench numbers unchanged or better; a deliberately-wrong-fps spawn
(pass 30 for a 60fps clip) plays at correct wall-clock speed (verify by
eye or by frame-content timestamps in testsrc2's burned-in counter).

## Phase 4 — contention & churn re-measurement ☐

Goal: decide with fresh data whether warm-pool behavior needs tuning
after Phase 1. **Measure first — no speculative changes.** The warm
design (full-quickview-res neighbors, serialized fills, promote-before-
prune) is settled in PLAN.md/CLAUDE.md; don't relitigate it, tune it.

1. Re-run pacebench while the app browses a 4K60-heavy directory
   (selection moves every ~1s) — or add a debug HUD/log of `take_frame`
   gap counts on the live sel stream. Question: do warm fills still cause
   visible gaps on the playing stream now that each fill costs ~1.5 cores
   instead of ~3.7?
2. If yes, options in preference order: reduce warm pool from 4 to 2
   (right, right+1) via a `Tuning` field; increase the settle delay for
   warm spawns; spawn warm decoders at reduced resolution in grid mode
   only (costs quickview-open latency for the promoted stream — weigh it).
3. Also check `Drop for LivePlayer` (`kill` + blocking `wait`) on the
   render thread during selection moves — up to 3 drops in one tick. If it
   shows up in gap traces, move reaping to a background thread.

Acceptance: browsing a 4K60 directory with live video shows no take_frame
gap >50ms attributable to warm fills (log-verified), and selection-move
feel is subjectively clean at trackpad speed.

## Phase 5 — DEFERRED: NV12 over the pipe + GPU color conversion

**Do not start until the entry criteria below are met.** Decision
2026-07-10: after Phase 1, the remaining CPU convert (~0.7 core at 4K60)
and pipe/upload bandwidth (14.7MB/frame) are no longer the bottleneck; the
win shrinks from "unlocks realtime" to "saves a fraction of a core", while
the risk is the project's known worst regression class: thumbs are decoded
to RGBA *via ffmpeg* precisely so live video and thumbnails share color
math (see `decode_jpeg`'s comment); a shader-side YUV→RGB must reproduce
BT.601/BT.709 × limited/full-range per clip or the thumb→live gamma pop
returns. It also touches the App boundary (`HiresFrame` carries RGBA
today), the renderer (two-plane R8+RG8 textures, new sampling path in
`tiles.wgsl`), and leaves the atlas hover lane RGBA regardless (two code
paths forever).

Entry criteria (any of):
- Profiling after Phases 1–4 shows pipe/upload/convert ≥1 core or causing
  measurable gaps on target hardware.
- The edge-peek quickview milestone lands (hires texture → array rework —
  do both texture-format disruptions in one renderer change).
- Multiple simultaneous hires streams become a requirement.

Sketch for when it happens: ffmpeg outputs `nv12` (5.5MB/frame at 1440p,
2.67× less than RGBA); upload as R8 (luma) + RG8 (chroma) planes; convert
in `tiles.wgsl` with matrix+range selected per clip from meta (probe
records `color_space`/`color_range` — add to `Meta` alongside Phase 1's
`pix_fmt`); verify with the PSNR-vs-thumbnail check and a
BT.601/BT.709/full-range clip matrix test before shipping.

---

## Standing rules for this work

- Benchmarks run **serially, on an idle machine, on AC power**; note the
  machine + ffmpeg version next to any number you record.
- Every phase re-runs the four `sb-media` regression tests
  (`live_player_paces_frames`, `unwatched_player_stalls_then_serves_instantly`,
  `dropped_player_releases_its_reader`, `anim_sheet_generates_and_tiles`).
- The pacing invariants from CLAUDE.md still bind: promotion before
  pruning; bounded queue = free warmth; drop must wake the condvar.
- `set_parked` currently has no call sites (focus-pause kills lanes
  instead). If it stays unused after Phase 4, delete it.
