# Baseline: validating `perf: cap gen-sweep concurrency while a live stream presents` (6f82ad9)

First real use of the Tier-A harness: an A/B of the gen-sweep-cap commit against
its own behavior reverted, isolating the commit's primary mechanism.

- **Scenario:** [`sweep_vs_playback.toml`](../scenarios/sweep_vs_playback.toml) —
  cold start, 61-clip corpus (one 1080p h264 played first, then 60 heavy 4K/60
  HEVC sweep clips). Open the first clip in quickview, hold 8s while the
  background thumbnail gen sweep grinds the 4K sources.
- **Corpus:** built by [`gen_corpus.sh`](../scenarios/gen_corpus.sh) — copies with
  distinct mtimes so each is its own `size_mtime` cache entry (= its own gen job).
- **Arms:** `capped` = HEAD; `uncapped` = HEAD with the three gen-sweep live
  protections reverted (concurrency cap → unlimited, background-band I/O
  `taskpolicy -b` → normal, sweep decode software → VideoToolbox).
- **Method:** 6 interleaved cold-cache reps per arm (alternated to dodge thermal
  drift). Local SSD, Apple Silicon.

## Result

| metric | capped (HEAD) | uncapped (parent) | Δ |
|---|--:|--:|--:|
| selected **spawn_to_served** p50 | 225 ms | 326 ms | **+101 ms (+45%)** |
| selected spawn_to_ready p50 | 217 ms | 314 ms | +97 ms |
| late_frames | 0 | 0 | 0 |
| reanchors | 0 | 0 | 0 |
| thumbs_cached (in 8s window) | 26–30 | 31 (every rep) | sweep clears faster uncapped |

Per-rep `selected/spawn_to_served` (ms): capped `207, 224, 225, 246, 259, 446`;
uncapped `305, 305, 326, 361, 363, 368`. The uncapped floor (305) sits above the
capped median (225) — every uncapped rep is slower to first frame than a typical
capped rep. Consistent, not noise.

## Reading

- **The cap works — on time-to-first-frame.** Uncapped, the selected stream's
  cold spawn races a full pool of concurrent 4K VideoToolbox extracts for the
  media engine and CPU, paying ~100 ms more to first frame every rep. The cap
  throttles the sweep to one software-decoded job while the stream comes up, and
  the clip opens ~45% faster.
- **Sustained playback is unaffected here** — `late_frames` / `reanchors` are flat
  zero in both arms. On this hardware the benefit is concentrated at *startup*,
  not mid-stream.
- **It costs sweep throughput, as designed** — uncapped clears all in-window
  thumbs every run; capped leaves a few unfinished. The intended "user attention
  wins CPU" trade, now quantified.

## Follow-up: the disk-band half, on a slow drive

The commit's headline pain is multi-second `live 0` **droughts on an
external/encrypted drive**, which the local-SSD run above can't touch (page-cache-warm
~20 MB files — the `taskpolicy -b` disk band has nothing to bite on). To chase it,
a second experiment read a real library over a **wifi/SMB NAS**, giving every run a
**disjoint slice of never-before-read files** so each sees genuinely cold drive I/O
(no `sudo purge` available). 6 reps per arm, disjoint files, cold.

| metric (cold wifi) | capped | uncapped |
|---|--:|--:|
| selected spawn_to_served — mean / max | 354 / 442 ms | **567 / 1445 ms** |
| late_frames — sum (reps affected) | 25 (2/6) | **46 (3/6)** |
| reanchors — sum | 25 | 46 |

**Cold reads off a slow drive DO make playback stutter in bursts**, and the cap
roughly halves both the dropped-frame count and the cold-spawn tail (1445 ms → 442 ms
worst case). But it's noisy (n=6; the played stream's own cold read dominates and the
cap only partially mitigates), so this is *suggestive, not conclusive*. More reps would
tighten it.

What could NOT be reproduced: the *severe* multi-second droughts. Likely because
(a) the only slow drives available are SSD-backed (encrypted USB) or SMB-cached (NAS) —
no HDD to seek-thrash, the most plausible drought mechanism; and (b) the heavy read
patterns the commit may have been reacting to (the bulk anim-sheet sweep, the full-clip
`fps=` decode) have since been **removed** (b6bcedf and earlier). The current thumb sweep
reads only a seeked keyframe region per clip — small — so it doesn't saturate a modern
drive enough to starve a low-bitrate stream.

### Methodology note (a harness fix this experiment forced)

The disk effect was **invisible in the first cold-wifi `compare.md`**: it aggregated
counters by **median**, and `late_frames` is mostly zero with occasional spikes, so the
median read `0` and the delta showed `+0.00`. Burst counters (late_frames, reanchors,
evictions) now aggregate by **mean** — the spikes are the signal. Steady per-run measures
(wall, frames, tick_ms) keep the outlier-robust median. Regression test:
`burst_counters_report_mean_not_median`.
