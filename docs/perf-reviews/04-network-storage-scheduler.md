# 04 — Scheduler on external & network storage (2026-07-24)

Follow-up to [03](03-slow-disk-scheduler.md). Review 03 measured a big library
on a direct-attached **USB HDD** and found the reported UI stall and crash did
**not** reproduce in Tier A, leaving three suspects (§6 there): Tier B GPU/upload,
scale beyond ~8.5k clips, and 4K-specific decode. This review takes the *other*
axis the user named — an **SMB network drive** — plus an **external SSD**
baseline, and it changes the headline:

**Over SMB, the render-thread stall reproduced.** A single `frame()` blocked
**1.49 s** during browsing and **1.04 s** while sitting on a live tile — each one
a synchronous filesystem call the render thread makes on the source volume. The
new `render_stall_*` instrument caught it and attributes it to the exact op. The
scheduler itself stayed healthy; the stall is the render thread touching a slow
disk directly, and one of the two op classes is entirely avoidable.

## Environment

| | |
|---|---|
| Commit | `e2d63e2` (feat(bench): scheduler/slow-disk instrumentation …), atop 03's `a8f3e88`. Working tree carried this review's added instruments (§1) — the runs measured them. |
| Machine | **M4 Mac mini (Apple M4 Pro, 14 cores, 48 GB RAM)**, macOS 26.5, host `pro.lan` |
| ffmpeg | 8.1.2 (Homebrew); rsmpeg in-process libav for live lanes |
| Binary | `cargo build --release` (debug inflates render cost ~10×, the quantity under test) |
| **Network corpus** | `/Volumes/Tower/Movies/Porn/Downloads` — **8,586 video candidates** (8,571 admitted, 15 rejected) across 657 subdirs, mounted `//nom@m4.local/Tower` over **SMB (smbfs) on gigabit LAN**, served by a second machine (`m4`) |
| SSD baseline | `/Volumes/Footage 1tb/Movies` — **465 mp4s, ~407 GB**, exFAT over USB, solid-state |
| Read-only | Both volumes read-only; every cache write went to a per-run temp `HOME`. |
| Raw data | `benchmarks/reports/04-slow-disk-scheduler/<run>/` (`summary.json` + gzipped `events.jsonl`; local-only per `benchmarks/.gitignore` — regenerate with the scenarios below). |

**SMB latency, sampled under sweep contention** (a Python probe run alongside a
live sweep, so representative of what the app's threads actually see):

| op | p50 | p95 | max |
|---|--:|--:|--:|
| `stat` | 13.0 ms | 22.7 ms | **622 ms** |
| open + 64 KB read | 59.8 ms | — | **1199 ms** |

Every finding below follows from that tail: an SMB `stat` normally costs ~13 ms
(vs ~12 ms on 03's HDD, ~0.04 ms on the SSD) but spikes past **600 ms**, and a
small read past **1.2 s**, whenever the link is busy. The pipeline does one
open-and-seek per file across thousands of files, so both the ingest walk and
any render-thread `stat` inherit that tail.

## 1. Instruments added on top of 03's set

03 added the worker-job / queue-depth / phase / ingest / process-canary
instruments. This review added three more (all always-on counters or runner
sampling; free when not recording):

- **`render_stall_*` counters** (`sb-media/src/probe.rs`, wired at the four
  render-thread fs sites in `sb-app/src/lib.rs`). 03's phase attribution blames a
  *phase* (`ns_live`); this names the *op class* and splits it so the fix target
  is unambiguous:
  - **`meta`** — `clip_meta` (source `stat` + `meta.json` read) at live-spawn.
    Load-bearing, but only the first read per clip (then memoized in `meta_cache`).
  - **`path`** — `cached_thumb_path` (source `stat`) for the drag ghost and the
    handoff-dump argument.

  `RenderStall::{Meta,ThumbPath}` tags each call; the aggregate
  `render_stalls`/`_us`/`_max_us` plus the two per-class pairs land in every
  `summary.json` and the `sb-bench` summary line.
- **Runner ergonomics** — `scroll` gained `n` (one event per frame, a sustained
  flick), and `inputs = ["$SB_BENCH_LIBRARY"]` resolves through the environment
  so a scenario targets a machine-local volume without hardcoding a mount point.
  (03's `sweep_drained` condition already anchors "the sweep finished".)

New scenarios: `benchmarks/scenarios/slow_disk_cold_sweep.toml` and
`slow_disk_browse_sweep.toml` (both take `$SB_BENCH_LIBRARY`).

## 2. Finding: the render thread stalls up to 1.5 s on SMB fs calls — the reported UI stall

This is the reproduction 03 could not get on the HDD. Two runs, two scenarios,
same mechanism:

| run | scenario | worst `frame()` | that frame is | `render_stall` total / max |
|---|---|--:|---|---|
| `net_browse_rel` | browse + quickview | **1490.7 ms** | 100 % in the `live` phase, = `render_stall_max` to 0.05 ms | 2539 ms / **1490.7 ms** (38 calls) |
| `net_cold_rel` | sit on a live tile | **1036.0 ms** | 100 % in the `live` phase, = `render_stall_max` | — / **1036.0 ms** |

In `net_browse_rel` the worst `frame()` (1490.7 ms), the worst `live`-phase frame
(1490.7 ms) and `render_stall_max_us` (1490.7 ms) agree to within 0.05 ms: **the
entire 1.49 s frame was one synchronous filesystem call** the render thread made
inside the live-update path. No GPU, no decode — just a `stat`/read on the SMB
volume that happened to hit the contended tail.

**It is intermittent.** A third browse run (`net_browse_rel2`, identical
scenario) hit a worst frame of only **11.9 ms** — the same code, a moment when
the link was quiet. That is exactly why a percentile hides it and why the
explicit `render_stall_max` / `tick_over` tail counters are the right instrument:
the stall is a fat tail on SMB latency, not a per-frame regression.

**Attribution** (from `net_browse_rel2`'s split, a run with ~5 lane spawns):

```
render stalls: meta 8× 32.1ms (clip_meta) | thumb_path 30× 20.7ms (cached_thumb_path)
```

- **`meta` (clip_meta)** — 8 calls, ~4 ms each here but the class that produced the
  1.49 s / 1.04 s spikes above (it does a `stat` *and* a `meta.json` read, and it
  runs at spawn, exactly when the disk is busiest). Load-bearing, but the read is
  already memoized after the first hit — it only needs to move **off** the render
  thread (it is a disk read inside `frame()`; the ingest thread pattern already
  exists for this).
- **`path` (cached_thumb_path)** — **30 calls = 6 per lane spawn**, which is the
  handoff-dump's `if served < 6` loop firing for both hover and selected lanes.
  These `stat` the source **even though `SB_HANDOFF_DUMP` is unset** — the
  argument is evaluated before `handoff_dump` early-returns. This is a
  diagnostic-only path doing up to 6 SMB `stat`s per spawn and producing nothing.
  **Pure waste; gate it behind the env check.**

## 3. Finding: ingest over SMB is the dominant cold-open cost — 5× the gatekeeper tax

03 §3 measured the gatekeeper's 16-byte header sniff at ~107 s to walk 8,586
files on the HDD. Over SMB it is far worse and it is measurable directly —
time to walk to 2,000 clips, gatekeeper on vs off, everything else equal:

| gatekeeper | wall to 2,000 clips | per-file |
|---|--:|--:|
| **on** (header sniff) | **187.8 s** | ~94 ms |
| **off** (stat + extension only) | **37.1 s** | ~18 ms |

**5.06×.** The header read adds ~76 ms per file over SMB (open + small read, the
1.2 s-tail op). Extrapolated to the full 8,586-file library that is **~11 minutes
of ingest-thread I/O for the header sniff alone**, to reject 0.17 % of files. In
`net_cold_rel2` the ingest thread was **I/O-bound for the entire 410 s run and
still only walked 3,446 of 8,586 files** (`ingest_io_us` = 410.4 s of 410.7 s
wall). This is on the ingest thread, so `tick_ms` never notices — but it is
minutes of a cold library trickling onto the grid, paid on every cold start
because nothing about it is cached (a warm re-walk is ~0.2 s).

This does not stall the UI, but it is the single biggest "the app feels broken on
a network library" cost, and it compounds with §2: the longer ingest runs, the
longer the window in which render-thread spawns keep hitting cold `clip_meta`
reads on a contended link.

## 4. Finding: the video thread re-anchors heavily but does not freeze

Even under SMB + sweep, the pacing layer held playback together:

| run | `late`/`reanchor` | served-frame gap p50 / p95 / max | gaps > 2 s |
|---|--:|--:|--:|
| `net_cold_rel2` (sit + watch, 410 s, ingest hammering the link) | **2463** | 16.8 / 105 / **109 ms** | 0 |
| `net_browse_rel` (browse) | 27 | 33 / 101 / **3202 ms** | 2 |

The sit-and-watch run re-anchored 2,463 times — every re-anchor is a frame that
came due late and the player reset its wall-clock anchor instead of accruing
debt — yet the **worst gap between two presented frames was 109 ms**. That is the
`SeekablePlayer` re-anchor invariant doing exactly its job: it converts a jittery
SMB read schedule into smooth-but-slightly-repaced playback rather than a
stutter-then-fast-forward. Browsing produced two >2 s gaps (a cold clip's first
frames arriving over a busy link at selection-change), but steady-state playback
never froze. **The "video thread stalls" half of the report did not reproduce as
a freeze** — it shows up as the re-anchor count, which is now the number to watch.

## 5. What did NOT reproduce (consistent with 03)

- **No memory or thread pathology.** `pending_bytes_peak` stayed **2.6–5.3 MB**
  across every SMB run — the unbounded result channel is nowhere near a crash
  mechanism, because the strict-priority pool cannot produce RGBA faster than the
  per-frame upload budget drains it. Threads plateaued at ~142, RSS peaked
  ~1.1 GB. A pool, not a leak.
- **No scheduler thrash.** Worker utilisation 0.35–0.99; a deep `gen` queue with
  `gen_running` pinned at 1 is the live throttle holding the sweep (03 §2), not a
  stall. Queue depths drained monotonically.
- **The SSD baseline is clean** (465 clips, exFAT USB): cold sweep + aggressive
  browse + uncapped-sweep A/B all showed tick p95 ≤ 0.20 ms, worst frame 2.7 ms,
  zero render stalls of note, ~0 backlog. The full library swept to `media_idle`
  in 68 s. SSD is simply not a slow disk — the reproduction needs the SMB tail.

## 6. Conclusions

1. **The reported "stalls UI threads" is real and is now located: render-thread
   filesystem calls on the source volume, not the task scheduler.** On fast
   storage they are sub-millisecond and invisible; on SMB they inherit a
   600 ms–1.2 s latency tail and block `frame()` for up to ~1.5 s. The scheduler,
   the result channel, and memory are all healthy — 03's instinct that the
   scheduler wasn't the culprit holds; the culprit was one layer over, on the
   render thread.
2. **Two fixes fall straight out of the attribution, neither applied here** (this
   session's remit was instrument + measure + report):
   - **Gate the handoff-dump `cached_thumb_path` behind `SB_HANDOFF_DUMP`.** It is
     6 SMB `stat`s per lane spawn for a diagnostic that is off. Zero-risk win
     (§2 `path` class).
   - **Move `clip_meta`'s first read off the render thread** (queue it like the
     reprobe path; spawn can proceed on the thumb's dims and adopt the meta when
     it lands). This removes the load-bearing `meta` class — the one that
     produced the 1.0–1.5 s spikes (§2 `meta` class).
3. **The gatekeeper header sniff should be optional per-volume or deferred.** 5×
   ingest cost over SMB (§3), ~11 min for the full library, to reject 0.17 % of
   files. 03 §3 flagged this on the HDD; SMB makes it the headline cold-open cost.
   Deferring the sniff to the first gen job (which opens the file anyway) would
   remove it from the critical path entirely.
4. **The gen throttle's permanent engagement** (03 §2) was visible here too
   (`gen_running` never > 1 while a tile is live) but is not implicated in the
   stall — leave it for the 4K re-measure 03 called for.

## 7. Open work

Indexed in [TASKS.md](../../TASKS.md); carries forward from [03 §Open work](03-slow-disk-scheduler.md#open-work).

- **Gate the handoff-dump source `stat`** (§2 `path`) — small, obvious, do first.
- **Move `clip_meta` off the render thread** (§2 `meta`) — the load-bearing stall;
  needs a spawn path that tolerates a not-yet-known meta.
- **Make the gatekeeper sniff optional/deferred per volume** (§3) — product call
  on 0.17 % junk vs minutes of cold-open I/O; SMB moves the needle.
- **Still unclosed from 03**: Tier B GPU/upload run (§6.1 there), long-run soak
  (§6.2), 4K decode path. This review closes 03's "scale/slow-disk" suspect for
  the UI-stall symptom (found on SMB) but not the crash — a soak on the network
  volume with the process canary armed is the next step for that.

---

Back to the [perf-review index](README.md) · [CLAUDE.md](../../CLAUDE.md)
