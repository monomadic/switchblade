# 04 — Scheduler on external/network storage: SSD baseline (+ SMB runs)

- **Date:** 2026-07-24
- **Commit:** `d521da1` ("config", master) + the instrumentation added in this session (uncommitted at run time; the runs below include it)
- **Machine:** M4 Mac mini (Apple M4 Pro, 14 cores, 48 GB RAM), macOS 26.5
- **ffmpeg:** 8.1.2 (Homebrew), rsmpeg in-process libav for live lanes
- **Corpus:** `/Volumes/Footage 1tb/Movies` — **465 real mp4s, ~407 GB**, exFAT over USB, solid-state. Read-only; all cache writes went to per-run temp `HOME`s. (A naive `find … -name '*.mp4'` reports 930 — half are AppleDouble `._*` stubs, which the ingest walk correctly skips as hidden files.)
- **Raw data:** `benchmarks/reports/04-slow-disk-scheduler/<run>/` (`summary.json` + `events.jsonl`; local-only per benchmarks/.gitignore, regenerate with the scenarios below).
- **Companion:** [03-slow-disk-scheduler.md](03-slow-disk-scheduler.md) measured a similar-size library on a direct-attached USB HDD with a parallel (richer) instrument set; this review is the external-SSD baseline, extended with the same corpus over SMB.

## Why

Reported symptom: with a big library on a slow disk the media task scheduler
"thrashes to the point it stalls UI threads and the video thread and crashes."
Nothing in the existing Tier-A instrumentation could see the suspected
mechanisms, so this session added instruments first, then measured.

## Instrumentation added (in this session)

All in the always-on counter style (relaxed atomics; free in normal runs):

| Instrument | Where | What it answers |
|---|---|---|
| `jobs_thumb/gen/anim/probe`, `worker_busy_us` | `sb-media` worker loop | Which queue tiers ran, and worker utilization (`busy / (3 × wall)`) |
| `results_sent/recv`, `result_bytes_sent/recv` | worker send / `try_recv` | Backlog in the **unbounded** worker→render result channel, in results and bytes — the memory-balloon gauge (payloads are ~1 MB decoded RGBA) |
| `render_stalls`, `render_stall_us`, `render_stall_max_us` | `sb-app` render thread | Blocking filesystem work **on the render thread**: `clip_meta` (source stat + meta.json read at live-spawn time) and `cached_thumb_path` (source stat; handoff-dump args + drag ghost). On a slow/network volume these are the direct UI-hitch mechanism |
| `MediaService::queue_depths()` → `sched_curve` in `summary.json` | bench rig, sampled per tick, change-compacted | Queue depth per tier + in-flight over time (thrash timeline) |
| `tick_spikes` (`[t, ms]` for every `frame()` ≥ 20 ms) | bench rig | Individual UI stalls — percentiles smear a single 300 ms freeze into nothing |
| `peak_result_backlog{,_bytes}` | bench rig | Worst channel backlog seen at any tick |
| `media_idle` condition | bench rig | "Every queue empty, nothing in flight" — lets a scenario time a full sweep (used by the cache primer) |
| `scroll` step gained `n` (one event per frame) | bench rig | Sustained trackpad flicks through a big library |
| `inputs = ["$SB_BENCH_LIBRARY"]` env expansion | bench rig | Machine-local corpora (external/network volumes) without hardcoding mount points |

New scenarios: `benchmarks/scenarios/slow_disk_cold_sweep.toml` (ingest +
sweep, no user attention) and `slow_disk_browse_sweep.toml` (scroll deep both
ways, keyboard browse, quickview + clip advances — all against the cold sweep).
Both take the corpus from `$SB_BENCH_LIBRARY`.

## Runs

All runs: 1280×800 viewport, animation `normal` (live video on) unless noted,
cold per-run cache unless noted. One run at a time (ffmpeg contention).

### 1. `slow_disk_cold_sweep` — cold cache, no interaction (55 s)

- Ingest of the whole volume closed in **~20 ms** (dir walk + per-file stat on
  exFAT USB is effectively free on this rig; the walk delivered all 465).
- Gen queue peaked at **417** (465 − first-screen thumbs) and drained
  monotonically at ~2.5 jobs/s.
- **The sweep ran 1-wide almost the whole run**: the selected tile presents
  live video at animation `normal`, so `gen_live_concurrency = 1` was engaged
  from first frame (`inflight` fell 3 → 1 at t≈1.4 s and stayed there). At
  this rate the 465-clip sweep needs ~3 min; uncapped (run 4) it takes 68 s.
- UI: tick p50 **0.06 ms**, p95 0.14 ms, max 2.14 ms, **zero** spikes ≥ 20 ms.
- Channel backlog peak: **1 result** (~0 MB). Render stalls: 9 calls,
  **0.4 ms total**, max 0.1 ms — the exFAT stat is ~40 µs here.

### 2. `slow_disk_browse_sweep` — browse + quickview during the cold sweep (49 s)

- 240 scroll events deep into the library and back, two selection moves,
  quickview open, 15 s of watching, two in-modal clip advances.
- Visible-thumb tier (tier 1) peaked at **42 queued** during the flicks and
  drained ahead of gen as designed; 170 thumb jobs vs 93 gen jobs.
- UI: tick p95 **0.17 ms**, max 2.73 ms, zero spikes. Render stalls: 38 calls,
  3.5 ms total, max 0.4 ms.
- Playback: `late_frames = 5`, `reanchors = 5` over the whole interactive run;
  promotions served the same tick (`promotion_to_served` p50 = 0 ms);
  `spawn_to_ready` p50 65 ms (in-process cold spawn against the USB disk).
- Reading note: `spawn_to_served` p50 ~5.9 s / max ~17 s for the selected lane
  is **not** a stall — warm lanes spawn parked by design and only serve when
  promoted, and promotion keeps the lane generation, so this metric spans the
  parked interval. `promotion_to_served` is the user-facing number.

### 3. Same, `gen_live_concurrency=0` (uncapped sweep racing playback)

- Sweep ran ~2.2× more work during the same script (209 vs 93 gen jobs;
  `worker_busy` 145.8 s vs 71.8 s — i.e. ~3.0 vs ~1.5 cores of niced sweep).
- **No measurable harm on this rig**: `late_frames` 5 → 5, `reanchors` 5 → 5,
  tick p95 0.16 ms, zero spikes, `spawn_to_ready` p50 66 ms.

### 4. `prime_cache` — full sweep to `media_idle`, animation `none`

- With no live stream the sweep runs 3-wide: **all 462 readable clips swept in
  68.5 s** (~6.8 jobs/s, ~3.0 cores busy). UI idle throughout (tick max 0.89 ms).
- `media_idle` (the new condition) fired exactly at queue drain — this is the
  anchor for "how long does the whole library take" claims.

### 5. Warm-cache browse (same script as run 2, primed cache)

- Worker busy collapsed 71.8 s → **5.4 s** (everything a cache hit).
- The only nonzero channel backlog of the session: **3 results / 2.6 MB** —
  cache-hit bursts deliver faster than the per-frame upload budget drains
  while the selected stream presents. This is the exact mechanism the
  `MEDIA_UPLOAD_BUDGET_LIVE` budget exists to spread out, now visible as a
  number. At 465 clips it is trivial; it scales with "cache-hit results
  arriving per second," so on a many-thousand-clip warm library a deep flick
  is the place to watch this gauge.
- `late_frames` still 5 — the 5 lates in runs 2/3 were **script-inherent
  (promotion/seek edges), not sweep contention**.

## Conclusions

1. **The reported thrash does not reproduce on this hardware + this volume at
   this scale.** On an M4 Pro with a USB SSD (even exFAT) and 465 clips, the
   scheduler is healthy under cold sweep + aggressive browsing: zero render
   spikes ≥ 20 ms, sub-millisecond render-thread fs work, effectively zero
   channel backlog, and playback undisturbed even with the sweep uncapped.
2. **The suspect mechanisms are now instrumented**, so the failing
   configuration will be attributable when we can run against it:
   - render-thread fs stalls (`render_stall_*`) — `clip_meta`'s source stat +
     meta.json read at spawn time is the render path's only disk touch; on a
     network volume each call is a network round-trip.
   - result-channel ballooning (`peak_result_backlog_bytes`) — the channel is
     unbounded with ~1 MB payloads; a stalled render loop + busy workers is
     the plausible memory-crash path.
   - queue-depth timeline (`sched_curve`) — shows tier starvation/pileup
     directly.
   - per-frame stalls (`tick_spikes`) — individual freezes, not percentiles.
3. **`gen_live_concurrency`'s default cap costs sweep time on fast disks**:
   the same library sweeps in 68 s uncapped-unwatched vs ~3 min while a live
   tile presents. On this rig the cap buys nothing (run 3 shows no harm
   uncapped) — but that is exactly the trade the cap makes for slow disks,
   where the drive (not CPU) is the contended resource. No change recommended
   until the slow-volume numbers exist.
4. **Ingest is not a factor on this volume** (20 ms for 465 files, off-thread
   by design), but the walk is the first thing to re-measure on the network
   drive — it stats every file.

## Next steps

- Re-run both `slow_disk_*` scenarios with `SB_BENCH_LIBRARY` pointed at the
  network drive (the "even more of a challenge" volume). Expect: inflated
  `render_stall_ms max` (clip_meta on spawn), inflated `spawn_to_ready`,
  visible-thumb tier pileup in `sched_curve`, and possibly a real
  `peak_backlog_mb`. Those numbers decide where the fix goes (e.g. moving
  `clip_meta`'s first read off the render thread, or bounding the result
  channel).
- If the crash is memory, `peak_result_backlog_bytes` plus `/usr/bin/time -l`
  peak RSS on the runner brackets it in one rep.
- The 5 script-inherent late frames in the browse scenario are worth a quick
  event-level look someday (they re-anchor and recover by design), but they
  are not the reported symptom.
