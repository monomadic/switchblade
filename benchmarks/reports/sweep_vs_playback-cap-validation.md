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

## The limit of this run (what it does NOT show)

The commit's headline pain — multi-second `live 0` **droughts on an
external/encrypted drive** — did not reproduce, because this ran on a local SSD
with page-cache-warm ~20 MB files. The `taskpolicy -b` disk-band half of the
commit had nothing to bite on. **This baseline validates the CPU + VT-media-engine
mechanism only.** The disk-starvation mechanism needs a slow-drive corpus
(external / network volume — Phase 5.1) or Tier B, tracked as follow-up.
