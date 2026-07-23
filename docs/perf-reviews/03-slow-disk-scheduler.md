# 03 — Scheduler under a big library on a slow disk (2026-07-24)

Reflective record of the 2026-07-24 investigation into "the task scheduler
thrashes on a big library on a slow disk, to the point it stalls the UI and
video threads and crashes." New instrumentation was added first (below), then
six measured runs against a real 8,571-clip library on an external USB HDD.

**Read the headline honestly: the reported UI stall and the crash did NOT
reproduce.** Tier A's render thread stayed clean in every run — worst frame
44.5 ms across all six, zero frames over 50 ms. What the instruments *did*
find is a scheduler that leaves ~40–45% of the drive's achievable throughput
unused for no measured benefit, and a cold-open cost dominated by a source
nobody was watching. Both are real; neither is the crash. §6 states precisely
what remains unexplained and what would be needed to catch it.

## Environment

| | |
|---|---|
| Commit | `2407470` — *fix(grid): load thumbs selection-first in reading order, not center-out* |
| Working tree | dirty — this review's instrumentation (§1) was added on top of `2407470` and is what the runs measured |
| Date | 2026-07-24 |
| Machine | **Mac mini, Apple M4 Pro, 12 cores, 24 GB RAM** |
| OS / ffmpeg | macOS 26.5.2 · ffmpeg 8.1.2 |
| Binary | `cargo build --release -p sb-app --bin sb-bench` — **release, not debug**. Debug inflates render-thread cost by roughly an order of magnitude, which is precisely the quantity under test |
| Library | `/Volumes/Tower/Movies/Porn/Downloads` — 19,289 files, of which **8,586 are video candidates, 8,571 admitted, 15 rejected** by the gatekeeper |
| Drive | 15 TB USB, APFS, spinning (`M000J-2TW103`, 4096 B blocks) |

**Machine load.** The first run (A, discarded) was contaminated by a Topaz
Video upscale — a job-runner encode the user's `~/.zsh/bin/topaz-encode`
script had picked up, at 110–250% CPU. It was reading the *internal* disk, not
the volume under test, but it cost ~1–2 cores. It was killed before run B; all
runs reported here (B–H) ran on a quiet machine. Investigating it also settled
a standing suspicion: **switchblade leaks no ffmpeg processes** — the only
`ffmpeg` on the box was Topaz's own, parented to the user's shell script, and
no orphaned homebrew ffmpeg or stale `sb-bench` existed.

### The drive is not uniformly slow — that distinction is the whole review

| Measurement | Result |
|---|---|
| Sequential read, large file, cold | **117–193 MB/s** |
| First cold read *under CPU contention* | 2.7 MB/s |
| Cold random seek + 64 KB read, 60 files sampled across the library | **p50 11.8 ms, p95 18.0 ms, max 19.1 ms** |

This is an ordinary HDD profile: fine when streaming, ~12 ms per head move.
Every cost below follows from that one number. The pipeline's work is
*seek-shaped* — one open-and-seek per file across thousands of files — so the
library's size enters as `8,586 × 12 ms ≈ 103 s` of pure head movement per
full pass, before a single frame is decoded.

## 1. What was added to the instrumentation

The existing probe measured *latencies of actions*. Nothing measured the
scheduler itself, so "the scheduler thrashes" was not a checkable claim. Added
(`crates/sb-media/src/probe.rs`, wired through `sb-media/src/lib.rs`,
`sb-app/src/{lib,ingest,bench}.rs`):

- **`JobStart` / `JobEnd` events** — every worker-pool job, tagged with its
  queue `Tier` (thumb / reprobe / chapters / anim / gen), a monotonic job id
  (so a start/end pair matches even when two workers hold the same clip), the
  wall time it occupied its worker, and an `Outcome`. Outcome splits `hit`
  (artifact already on disk, no source I/O) from `made` (paid the seek) from
  `failed` — pooling those makes a slow-disk run look fast in exactly the case
  where it isn't.
- **`QueueDepths`** — per-tier queue depth, in-flight count, `gen_running`,
  and merged-request debt, sampled by the runner every tick and compacted to
  changes. This is what made §2 visible; no latency metric can show it.
- **Worker utilisation** — `worker_busy_us` against wall × `WORKERS`. The one
  number that separates "the sweep is saturating the pool" from "the pool is
  parked on a throttle."
- **Result-backlog canary** — `pending_results` / `pending_bytes` /
  `pending_bytes_peak` around the unbounded result channel, which carries
  decoded RGBA (~0.9 MB per thumb). This was a leading crash hypothesis.
- **Ingest counters** — `ingest_seen` / `admitted` / `rejected`, and
  `ingest_io_us` timing the ingest thread's `stat`, `read_dir` and the
  gatekeeper's header read. §3 is entirely this instrument.
- **Render-thread phase attribution** — `ns_ingest_install` / `ns_drain_media`
  / `ns_live` accumulators, sampled as per-tick deltas, so a stalled frame is
  attributable to a phase instead of arriving as one opaque `tick_ms`.
- **Stall-tail counts** — frames over 16/50/100/250/1000 ms, counted
  explicitly. A p95 of 1.6 ms hides the four 900 ms frames that would *be* the
  complaint; percentiles summarize the tail away by construction.
- **Process canary** — RSS (process tree, including ffmpeg children) and
  thread count, sampled at 1 Hz from a helper thread via `ps`, never on the
  render thread. No new dependency: macOS `ps` has no `thcount` keyword, so
  the thread count is `ps -M` rows minus one.
- **Served-frame gaps** — see §5; this one required fixing the event itself.

Two instrumentation bugs were found *by* the first runs and fixed before B–H:
`ingest_seen` counted only `handle_path` and not the recursive walk (so every
directory-based run reported an empty ingest), and the `ps` sampler used a
keyword macOS doesn't have (so `proc_curve` was silently empty).

## 2. Finding: the gen throttle is permanently engaged, and buys nothing here

`gen_live_concurrency` (default `1`) narrows the background sweep to one
concurrent job "while the selected stream presents." Its doc comment promises
*"Nothing playing → the full pool always runs."*

**That condition never occurs in normal grid use.** The throttle keys on
`watching = live_sel.is_some() && !parked && !paused`, and the grid's selected
tile always has a live lane. Measured over run B — 177 s, nothing deliberately
being watched, no quickview:

```
gen_running distribution over samples: {0: 294, 1: 4698}
inflight    distribution over samples: {1: 4523, 2: 158, 3: 311}
worker_utilisation: 0.38
```

`gen_running` never once reached 2. Two of three workers were idle for 91% of
the run while the gen queue stood at 8,155–8,321.

Sweeping the knob, same scenario, same 120 s, both with warm directory
metadata (runs D and C):

| | `gen_live_concurrency=1` (default) | `=0` (uncapped) |
|---|--:|--:|
| Thumbs cached in 120 s | 375 | **611** (+63%) |
| Worker utilisation | 0.36 | **0.99** |
| gen job p50 / p95 | 322 / 825 ms | 505 / 1325 ms |
| gen job max | 1,469 ms | 3,419 ms |
| RSS peak | 762 MB | 1,614 MB |

The drive *does* degrade under concurrency — per-job cost rises 57%, so three
workers return 1.63× not 3×. But throttling to one leaves ~40% of achievable
throughput unused.

The throttle exists to protect playback, so the decisive test is whether
uncapping costs the watched stream anything (runs F and G,
`slow_disk_watch_under_sweep`, quickview open and playing for 60 s with the
sweep loose):

| | throttle on | throttle off |
|---|--:|--:|
| Served-frame gap p50 | 16.8 ms | 16.7 ms |
| Served-frame gap p95 | 33.6 ms | 33.6 ms |
| Served-frame gap max | 41.7 ms | 43.0 ms |
| `late_frames` / `reanchors` | 1 / 1 | 1 / 1 |
| Thumbs cached in 110 s | 353 | **631** (+79%) |

**Playback is indistinguishable; the sweep is 1.79× faster.** On this drive
the throttle costs 44% of sweep throughput and buys nothing measurable.

The caveat that keeps this from being a settled verdict: the clips these runs
happened to select were 1080p h264 at 1–3 Mbps. The throttle was originally
justified by a 4K cold-spawn measurement (`gen_live_concurrency`'s doc cites
~45% slower time-to-first-frame uncapped), and a 4K60 stream needs an order of
magnitude more sustained bandwidth than what was tested here. **Do not delete
the throttle on this evidence.** What the evidence does support is that its
trigger condition is wrong: it is meant to engage while a stream is being
*watched*, and instead it engages permanently.

## 3. Finding: the gatekeeper's header sniff owns the cold open

Run B, cold page cache, whole library:

```
ingest: seen=8586 admitted=8571 rejected=15 io=106.9s
```

**106.9 seconds of ingest-thread I/O** — against a 177 s run. The gatekeeper
reads 16 bytes from the head of every candidate file to check its container
signature; on this drive each of those is a ~12 ms seek. It rejected **15
files out of 8,586** (0.17%).

This is not a render-thread stall — the work is correctly on the ingest thread
and `tick_ms` never noticed. But it is ~107 s during which a cold library
trickles onto the grid, and the cost is paid on *every* cold start because
nothing about it is cached. Whether 0.17% of junk files is worth ~107 s of
head movement on an HDD library is a product call, not a performance one; the
measurement is here so it can be made. (Note the ordering interaction: with
`--sort newest`, arrivals must merge as they stream, so a slow trickle is also
a long window of grid reflow.)

Once metadata is warm, the same walk costs **0.2 s** — so this is strictly a
cold-start cost, and re-running a scenario back-to-back will not show it. Runs
C–H all had warm metadata; only B measures this honestly.

## 4. Finding: one file can pin one third of the pool for 12–16 seconds

Per-job costs, cold, from the new `JobEnd` events (run B):

| tier | outcome | n | p50 | p95 | **max** | worker-s |
|---|---|--:|--:|--:|--:|--:|
| gen | made | 333 | 369 ms | 922 ms | **11,889 ms** | 165.4 |
| thumb | made | 78 | 289 ms | 556 ms | **14,099 ms** | 37.6 |
| gen | failed | 1 | 131 ms | — | 131 ms | 0.1 |
| gen | hit | 3 | 0 ms | — | 0 ms | 0.0 |

Run A (under CPU contention) recorded a single `gen` job at **32.5 seconds**.

A `thumb`-tier job is a *visible tile* — so on a cold big library a tile can
take 14 s to appear, which is very likely part of what reads as "the app is
broken." Note this compounds with §2: with the sweep capped at one concurrent
job, one 12 s file stalls the entire background sweep for 12 s, because the
one running job *is* the sweep.

Cache hits are ~0 ms for gen and ~20 ms for thumb (the JPEG decode), so a warm
library costs essentially nothing — the whole problem is the first pass.

## 5. The video-thread instrument was broken, and the fix is the finding

`FrameServed` was emitted only for a lane's **first** frame
(`if live.first_frame.is_none()`), for both the selected and hover lanes. Run
E produced **2 `frame_served` events in 110 seconds** of playback — which
looks exactly like a catastrophic video stall and is in fact an
under-instrumented event.

This matters beyond bookkeeping: the app's own tick times stay clean while
playback freezes, because the decoder is a separate thread. The *only*
in-process view of a starved decoder is the gap between consecutive served
frames, and that event did not exist. It does now (emitted per served frame,
still free when recording is disarmed — one relaxed load, the closure never
runs). `spawn_to_served` is unaffected: `compute_latencies` already took the
first sample per lane generation.

With it working, playback under a loose sweep is healthy (runs F/G, §2): p50
16.8 ms, p95 33.6 ms, max 43.0 ms, 5,617 gaps sampled — essentially every
frame delivered on time. Note the runner's 60 Hz vsync stand-in is the floor
here, so p50 ≈ 16.7 ms means "as fast as the harness asks," not a measured
frame rate.

## 6. What did NOT reproduce, and what that narrows

**No render-thread stall.** Across all six runs, including a real trackpad
pinch stream and two hard pans over 8,571 tiles with the sweep running and
live lanes up (run H, `slow_disk_gesture_under_sweep`):

| run | tick p50 | tick p95 | tick max | >16 ms | >50 ms | >250 ms |
|---|--:|--:|--:|--:|--:|--:|
| B big library | 0.19 ms | 1.59 ms | 3.3 ms | 0 | 0 | 0 |
| C uncapped | 0.13 | 0.38 | 3.8 | 0 | 0 | 0 |
| D capped | 0.15 | 0.48 | 5.4 | 0 | 0 | 0 |
| F watch | 0.05 | 0.30 | 5.4 | 0 | 0 | 0 |
| G watch uncapped | 0.04 | 0.25 | 3.9 | 0 | 0 | 0 |
| **H gesture** | 0.10 | 2.70 | **44.5** | **1** | 0 | 0 |

The gesture run's single 44.5 ms frame is the release walk, and it is nowhere
near the "O(library²) freeze" shape `pinch_ribbon`'s prose warns about. The
`Nothing in the gesture path may be O(library) per frame` invariant is holding
at 8,571 tiles.

**No memory or thread pathology.** `pending_bytes_peak` stayed at **1.8 MB**
across every run — the unbounded result channel is nowhere near a crash
mechanism, because the strict-priority pool simply cannot produce RGBA faster
than a 64-per-frame budget drains it. Threads plateau at **121–123 within 12 s
and stay flat** for the rest of the run: a pool, not a leak. RSS peaked at
714–780 MB throttled, 1,614 MB uncapped.

**So where is the reported stall and crash?** Three suspects survive, in order
of how much I'd bet on them:

1. **Tier B — GPU, upload and present.** Tier A has no GPU and no present, and
   `benchmarks/HARNESS.md` §0.5 explicitly assigns upload stalls,
   present-to-present gaps and visible jank to Tier B. Run H's atlas churned
   hard during the pans (**209 evictions**), and every one of those is a
   texture write Tier A stages and discards. A clean Tier A gesture run does
   not clear the gesture — it narrows the suspect to exactly the half Tier A
   cannot see. **This is the next place to look.**
2. **Scale beyond what was tested.** These runs measured the first ~180 s of a
   sweep that needs ~1–2 hours to finish, and the library was 8,571 clips. A
   crash after 40 minutes, or at 50k clips, is entirely consistent with
   everything above.
3. **4K clips specifically.** The clips the scenarios happened to land on were
   1080p h264. Both the throttle's original justification and the app's
   heaviest decode path are about 4K60, and neither was exercised.

## 7. Instruments to reach for next time

`slow_disk_big_library` / `slow_disk_watch_under_sweep` /
`slow_disk_gesture_under_sweep` are committed under
`benchmarks/scenarios/`. They point at a **user-owned, machine-specific
library path** and will not run elsewhere unchanged — edit `[setup].inputs`.
The library is read-only in every run; the app writes only to the harness's
temp `HOME` cache.

- Cold vs warm is now a first-class distinction: `jobs_hit` vs `jobs_made`,
  and `ingest_io_us`. A repeat run of any of these scenarios has warm
  directory metadata and is **not** a cold-start measurement. Only the first
  run after a reboot (or a genuinely untouched library) measures §3.
- `worker_utilisation` is the fastest read on whether the pool is working or
  parked; pair it with `gen_running` from `queue_curve` before theorizing.
- Sweep the throttle with `--set gen_live_concurrency=0` — it needs no rebuild
  and self-documents in the report header.

Raw bundles (`summary.json` + gzipped `events.jsonl`) for runs B, C, D, F, G,
H are committed under
[benchmarks/reports/slow-disk-scheduler/](../../benchmarks/reports/slow-disk-scheduler/).

## Open work

Indexed in [TASKS.md](../../TASKS.md); full problem statements are above.

- **The gen throttle's trigger condition is wrong** (§2) — it is meant to
  protect a *watched* stream and instead engages permanently, costing ~40–45%
  of sweep throughput. Needs a condition that distinguishes "the user is
  watching this" from "a selected tile has a live lane." Re-measure on 4K
  before touching the cap's value.
- **Tier B run of the gesture and sweep scenarios** (§6.1) — the only
  remaining in-house place the reported UI stall can hide.
- **Gatekeeper cold-open cost** (§3) — 107 s of head movement to reject 0.17%
  of files. Product call: make it optional per-volume, defer it to the first
  gen job (which opens the file anyway), or accept it.
- **Long-run soak** (§6.2) — let a full sweep finish (~1–2 h) with the process
  canary armed, to catch a crash these 2–3 minute windows cannot.

---

Back to the [perf-review index](README.md) · [CLAUDE.md](../../CLAUDE.md) · [DESIGN.md](../../DESIGN.md)
